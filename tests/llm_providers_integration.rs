//! Integration tests for LLM provider config API (`/api/users/me/llm-providers`).

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{create_user, test_router, test_state};

// ---------------------------------------------------------------------------
// Create provider
// ---------------------------------------------------------------------------

/// POST creates a provider config and returns the ID. No `env_vars` in response.
#[sqlx::test(migrations = "./migrations")]
async fn create_provider(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmuser1", "llmuser1@test.com").await;

    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": "My Bedrock",
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA1234567890ABCDEF",
                "AWS_SECRET_ACCESS_KEY": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            },
            "model": "claude-sonnet-4-20250514",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create provider failed: {body}"
    );
    assert!(body["id"].as_str().is_some(), "response should contain id");
    // env_vars must NOT leak in the response
    assert!(
        body.get("env_vars").is_none(),
        "env_vars must not be in create response"
    );
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

/// Oversized label and invalid `provider_type` are rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_provider_validates_input(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmval1", "llmval1@test.com").await;

    // Label too long (>255)
    let long_label = "x".repeat(256);
    let (status, _) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": long_label,
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA1234567890ABCDEF",
                "AWS_SECRET_ACCESS_KEY": "secret",
            },
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "oversized label should be rejected"
    );

    // Invalid provider_type
    let (status, _) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "openai",
            "label": "bad type",
            "env_vars": {},
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "invalid provider_type should be rejected"
    );

    // Missing required env_vars for bedrock
    let (status, _) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": "missing keys",
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA1234567890ABCDEF",
                // Missing AWS_SECRET_ACCESS_KEY
            },
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing required env_vars should be rejected"
    );
}

// ---------------------------------------------------------------------------
// List providers
// ---------------------------------------------------------------------------

/// Create 2 providers, list, verify count.
#[sqlx::test(migrations = "./migrations")]
async fn list_providers(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmlist1", "llmlist1@test.com").await;

    // Create two providers
    let (s1, _) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": "Bedrock 1",
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA1111111111111111",
                "AWS_SECRET_ACCESS_KEY": "secret1111111111111111111111111111111111",
            },
        }),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, _) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "vertex",
            "label": "Vertex 1",
            "env_vars": {
                "ANTHROPIC_VERTEX_PROJECT_ID": "my-gcp-project",
            },
        }),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);

    // List
    let (status, body) = helpers::get_json(&app, &token, "/api/users/me/llm-providers").await;
    assert_eq!(status, StatusCode::OK, "list providers failed: {body}");
    let items = body["items"]
        .as_array()
        .expect("response should have items array");
    assert_eq!(items.len(), 2, "should have exactly 2 providers");

    // Verify fields present on each item (metadata only, no secrets)
    for item in items {
        assert!(item["id"].as_str().is_some());
        assert!(item["provider_type"].as_str().is_some());
        assert!(item["label"].as_str().is_some());
        assert!(item["validation_status"].as_str().is_some());
        assert!(
            item.get("env_vars").is_none(),
            "env_vars must not be in list response"
        );
    }
}

// ---------------------------------------------------------------------------
// Update provider
// ---------------------------------------------------------------------------

/// PUT updates a provider's label and `env_vars`.
#[sqlx::test(migrations = "./migrations")]
async fn update_provider(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmupd1", "llmupd1@test.com").await;

    // Create
    let (_, create_body) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": "Original Label",
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA2222222222222222",
                "AWS_SECRET_ACCESS_KEY": "secret2222222222222222222222222222222222",
            },
        }),
    )
    .await;
    let provider_id = create_body["id"].as_str().unwrap();

    // Update label and env_vars
    let (status, _) = helpers::put_json(
        &app,
        &token,
        &format!("/api/users/me/llm-providers/{provider_id}"),
        serde_json::json!({
            "label": "Updated Label",
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA3333333333333333",
                "AWS_SECRET_ACCESS_KEY": "secret3333333333333333333333333333333333",
            },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "update should succeed");

    // Verify via list
    let (_, list_body) = helpers::get_json(&app, &token, "/api/users/me/llm-providers").await;
    let items = list_body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["label"], "Updated Label");
    // validation_status should reset to untested after update
    assert_eq!(items[0]["validation_status"], "untested");
}

// ---------------------------------------------------------------------------
// Delete provider
// ---------------------------------------------------------------------------

/// DELETE removes a provider, verified via list.
#[sqlx::test(migrations = "./migrations")]
async fn delete_provider(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmdel1", "llmdel1@test.com").await;

    // Create
    let (_, create_body) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "custom_endpoint",
            "label": "To Delete",
            "env_vars": {
                "ANTHROPIC_BASE_URL": "https://example.com/v1",
                "ANTHROPIC_API_KEY": "sk-custom-key-12345",
            },
        }),
    )
    .await;
    let provider_id = create_body["id"].as_str().unwrap();

    // Delete
    let (status, _) = helpers::delete_json(
        &app,
        &token,
        &format!("/api/users/me/llm-providers/{provider_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "delete should succeed");

    // Verify gone
    let (_, list_body) = helpers::get_json(&app, &token, "/api/users/me/llm-providers").await;
    let items = list_body["items"].as_array().unwrap();
    assert!(items.is_empty(), "provider should be gone after delete");
}

// ---------------------------------------------------------------------------
// User scoping (isolation)
// ---------------------------------------------------------------------------

/// User B cannot see User A's providers.
#[sqlx::test(migrations = "./migrations")]
async fn provider_scoped_to_user(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_a_id, token_a) =
        create_user(&app, &admin_token, "llmscope_a", "llmscope_a@test.com").await;
    let (_user_b_id, token_b) =
        create_user(&app, &admin_token, "llmscope_b", "llmscope_b@test.com").await;

    // User A creates a provider
    let (status, create_body) = helpers::post_json(
        &app,
        &token_a,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "azure_foundry",
            "label": "User A Provider",
            "env_vars": {
                "ANTHROPIC_FOUNDRY_API_KEY": "foundry-key-aaaaa",
            },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let provider_id = create_body["id"].as_str().unwrap();

    // User B lists — should see zero
    let (status, body) = helpers::get_json(&app, &token_b, "/api/users/me/llm-providers").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert!(items.is_empty(), "user B should see no providers");

    // User B tries to update user A's provider — should get 404 (ownership check)
    let (status, _) = helpers::put_json(
        &app,
        &token_b,
        &format!("/api/users/me/llm-providers/{provider_id}"),
        serde_json::json!({
            "label": "Hijacked",
            "env_vars": {
                "ANTHROPIC_FOUNDRY_API_KEY": "stolen-key",
            },
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "user B should not update user A's provider"
    );

    // User B tries to delete user A's provider — should get 404
    let (status, _) = helpers::delete_json(
        &app,
        &token_b,
        &format!("/api/users/me/llm-providers/{provider_id}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "user B should not delete user A's provider"
    );

    // User A can still see their provider
    let (_, body) = helpers::get_json(&app, &token_a, "/api/users/me/llm-providers").await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "user A should still have their provider");
}

// ---------------------------------------------------------------------------
// Active provider — set and get
// ---------------------------------------------------------------------------

/// Set active provider to "auto", get it back, verify match.
#[sqlx::test(migrations = "./migrations")]
async fn set_and_get_active_provider(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmact1", "llmact1@test.com").await;

    // Default should be "auto"
    let (status, body) = helpers::get_json(&app, &token, "/api/users/me/active-provider").await;
    assert_eq!(status, StatusCode::OK, "get active provider: {body}");
    assert_eq!(body["provider"], "auto", "default should be auto");

    // Set to "global"
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": "global" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "set to global should succeed"
    );

    // Verify it changed
    let (status, body) = helpers::get_json(&app, &token, "/api/users/me/active-provider").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["provider"], "global");

    // Set back to "auto"
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": "auto" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = helpers::get_json(&app, &token, "/api/users/me/active-provider").await;
    assert_eq!(body["provider"], "auto");
}

// ---------------------------------------------------------------------------
// Active provider — invalid values
// ---------------------------------------------------------------------------

/// Setting active provider to an invalid value returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn set_active_invalid_provider(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmbad1", "llmbad1@test.com").await;

    // Completely invalid value
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": "openai" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "invalid provider value should be rejected"
    );

    // "oauth" without OAuth credentials configured
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": "oauth" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "oauth without credentials should be rejected"
    );

    // "api_key" without Anthropic API key configured
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": "api_key" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "api_key without key should be rejected"
    );

    // "custom:<invalid-uuid>"
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": "custom:not-a-uuid" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "custom with invalid UUID should be rejected"
    );

    // "custom:<valid-uuid-but-nonexistent>"
    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": format!("custom:{fake_id}") }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "custom with nonexistent config should be 404"
    );
}

// ---------------------------------------------------------------------------
// Active provider — custom config requires validation
// ---------------------------------------------------------------------------

/// Setting active to a custom config that has not been validated returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn set_active_custom_requires_validation(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) =
        create_user(&app, &admin_token, "llmcustval", "llmcustval@test.com").await;

    // Create a provider config (validation_status defaults to "untested")
    let (_, create_body) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "custom_endpoint",
            "label": "Unvalidated",
            "env_vars": {
                "ANTHROPIC_BASE_URL": "https://custom.example.com",
                "ANTHROPIC_API_KEY": "sk-custom-key-99999",
            },
        }),
    )
    .await;
    let config_id = create_body["id"].as_str().unwrap();

    // Try to set it as active — should fail because it's "untested"
    let (status, body) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": format!("custom:{config_id}") }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "untested config should not be activatable: {body}"
    );
}

// ---------------------------------------------------------------------------
// Delete provider reverts active to "auto"
// ---------------------------------------------------------------------------

/// Deleting the active custom provider reverts `active_llm_provider` to "auto".
#[sqlx::test(migrations = "./migrations")]
async fn delete_active_provider_reverts_to_auto(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_user_id, token) =
        create_user(&app, &admin_token, "llmrevert", "llmrevert@test.com").await;

    // Create a provider config
    let (_, create_body) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": "Active Then Deleted",
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA4444444444444444",
                "AWS_SECRET_ACCESS_KEY": "secret4444444444444444444444444444444444",
            },
        }),
    )
    .await;
    let config_id = create_body["id"].as_str().unwrap();
    let config_uuid = Uuid::parse_str(config_id).unwrap();

    // Manually mark it as "valid" so we can set it active
    sqlx::query("UPDATE llm_provider_configs SET validation_status = 'valid' WHERE id = $1")
        .bind(config_uuid)
        .execute(&pool)
        .await
        .unwrap();

    // Set it as active
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": format!("custom:{config_id}") }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it is active
    let (_, body) = helpers::get_json(&app, &token, "/api/users/me/active-provider").await;
    assert_eq!(body["provider"], format!("custom:{config_id}"));

    // Delete the provider
    let (status, _) = helpers::delete_json(
        &app,
        &token,
        &format!("/api/users/me/llm-providers/{config_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Active provider should revert to "auto"
    let (_, body) = helpers::get_json(&app, &token, "/api/users/me/active-provider").await;
    assert_eq!(
        body["provider"], "auto",
        "active should revert to auto after deleting the active custom config"
    );
}

// ---------------------------------------------------------------------------
// Delete nonexistent provider
// ---------------------------------------------------------------------------

/// DELETE on a nonexistent provider returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn delete_nonexistent_provider(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) =
        create_user(&app, &admin_token, "llmdel404", "llmdel404@test.com").await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::delete_json(
        &app,
        &token,
        &format!("/api/users/me/llm-providers/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Update nonexistent provider
// ---------------------------------------------------------------------------

/// PUT on a nonexistent provider returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn update_nonexistent_provider(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) =
        create_user(&app, &admin_token, "llmupd404", "llmupd404@test.com").await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::put_json(
        &app,
        &token,
        &format!("/api/users/me/llm-providers/{fake_id}"),
        serde_json::json!({
            "label": "Ghost",
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA0000000000000000",
                "AWS_SECRET_ACCESS_KEY": "secret0000000000000000000000000000000000",
            },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Auth required
// ---------------------------------------------------------------------------

/// Requests without a token return 401.
#[sqlx::test(migrations = "./migrations")]
async fn unauthenticated_returns_401(pool: PgPool) {
    let (state, _admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, _) = helpers::get_json(&app, "", "/api/users/me/llm-providers").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": "no auth",
            "env_vars": {},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = helpers::get_json(&app, "", "/api/users/me/active-provider").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = helpers::put_json(
        &app,
        "",
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": "auto" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Too many env vars
// ---------------------------------------------------------------------------

/// Creating a provider with more than 50 env vars returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn create_provider_too_many_env_vars(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) =
        create_user(&app, &admin_token, "llmenvmax", "llmenvmax@test.com").await;

    // Build 51 env vars
    let mut env_vars = serde_json::Map::new();
    for i in 0..51 {
        env_vars.insert(format!("ENV_VAR_{i}"), serde_json::json!(format!("val{i}")));
    }

    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": "too many vars",
            "env_vars": env_vars,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "too many env vars should be rejected: {body}"
    );
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("too many env vars"),
        "error should mention too many env vars: {body}"
    );
}

// ---------------------------------------------------------------------------
// Get single provider
// ---------------------------------------------------------------------------

/// GET single provider returns metadata.
#[sqlx::test(migrations = "./migrations")]
async fn get_single_provider(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmget1", "llmget1@test.com").await;

    // Create a provider
    let (_, create_body) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": "Get Me",
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA5555555555555555",
                "AWS_SECRET_ACCESS_KEY": "secret5555555555555555555555555555555555",
            },
            "model": "claude-sonnet-4-20250514",
        }),
    )
    .await;
    let provider_id = create_body["id"].as_str().unwrap();

    // Get single provider
    let (status, body) = helpers::get_json(
        &app,
        &token,
        &format!("/api/users/me/llm-providers/{provider_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get provider failed: {body}");
    assert_eq!(body["id"], provider_id);
    assert_eq!(body["provider_type"], "bedrock");
    assert_eq!(body["label"], "Get Me");
    assert_eq!(body["model"], "claude-sonnet-4-20250514");
    assert_eq!(body["validation_status"], "untested");
    // env_vars must not leak
    assert!(
        body.get("env_vars").is_none(),
        "env_vars must not be in get response"
    );
}

/// GET single provider for nonexistent id returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_single_provider_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) =
        create_user(&app, &admin_token, "llmget404", "llmget404@test.com").await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &token,
        &format!("/api/users/me/llm-providers/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// GET single provider owned by another user returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_single_provider_wrong_user(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_a_id, token_a) =
        create_user(&app, &admin_token, "llmget-a", "llmget-a@test.com").await;
    let (_user_b_id, token_b) =
        create_user(&app, &admin_token, "llmget-b", "llmget-b@test.com").await;

    // User A creates a provider
    let (_, create_body) = helpers::post_json(
        &app,
        &token_a,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "vertex",
            "label": "A's provider",
            "env_vars": {
                "ANTHROPIC_VERTEX_PROJECT_ID": "my-project",
            },
        }),
    )
    .await;
    let provider_id = create_body["id"].as_str().unwrap();

    // User B tries to get it
    let (status, _) = helpers::get_json(
        &app,
        &token_b,
        &format!("/api/users/me/llm-providers/{provider_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Set active to custom — validated config
// ---------------------------------------------------------------------------

/// Setting active to a custom config that has been validated succeeds.
#[sqlx::test(migrations = "./migrations")]
async fn set_active_custom_validated_config_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_user_id, token) =
        create_user(&app, &admin_token, "llmcustok", "llmcustok@test.com").await;

    // Create a provider config
    let (_, create_body) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "custom_endpoint",
            "label": "Validated Custom",
            "env_vars": {
                "ANTHROPIC_BASE_URL": "https://custom.example.com",
                "ANTHROPIC_API_KEY": "sk-custom-validated-key",
            },
        }),
    )
    .await;
    let config_id = create_body["id"].as_str().unwrap();
    let config_uuid = Uuid::parse_str(config_id).unwrap();

    // Mark as validated
    sqlx::query("UPDATE llm_provider_configs SET validation_status = 'valid' WHERE id = $1")
        .bind(config_uuid)
        .execute(&pool)
        .await
        .unwrap();

    // Set as active — should succeed
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": format!("custom:{config_id}") }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "validated custom config should be activatable"
    );

    // Verify it is now active and includes provider_type and label
    let (status, body) = helpers::get_json(&app, &token, "/api/users/me/active-provider").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["provider"], format!("custom:{config_id}"));
    assert_eq!(body["provider_type"], "custom_endpoint");
    assert_eq!(body["label"], "Validated Custom");
}

// ---------------------------------------------------------------------------
// Active provider response structure
// ---------------------------------------------------------------------------

/// GET active-provider includes `has_oauth` and `has_api_key` flags.
#[sqlx::test(migrations = "./migrations")]
async fn get_active_provider_includes_flags(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmflags", "llmflags@test.com").await;

    let (status, body) = helpers::get_json(&app, &token, "/api/users/me/active-provider").await;
    assert_eq!(status, StatusCode::OK);

    // By default, new user has no OAuth or API key
    assert_eq!(body["has_oauth"], false);
    assert_eq!(body["has_api_key"], false);
    assert!(body["custom_configs"].as_array().unwrap().is_empty());
}

/// GET active-provider after creating a custom config includes it.
#[sqlx::test(migrations = "./migrations")]
async fn get_active_provider_includes_custom_configs(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmcust2", "llmcust2@test.com").await;

    // Create a provider config
    helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": "My Bedrock Config",
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA6666666666666666",
                "AWS_SECRET_ACCESS_KEY": "secret6666666666666666666666666666666666",
            },
        }),
    )
    .await;

    let (status, body) = helpers::get_json(&app, &token, "/api/users/me/active-provider").await;
    assert_eq!(status, StatusCode::OK);
    let configs = body["custom_configs"].as_array().unwrap();
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0]["provider_type"], "bedrock");
    assert_eq!(configs[0]["label"], "My Bedrock Config");
}

// ---------------------------------------------------------------------------
// Set active with api_key after configuring key
// ---------------------------------------------------------------------------

/// Setting active to `api_key` after configuring an Anthropic API key succeeds.
#[sqlx::test(migrations = "./migrations")]
async fn set_active_api_key_with_configured_key(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "llmapi1", "llmapi1@test.com").await;

    // Set an Anthropic API key for the user
    helpers::set_user_api_key(&pool, user_id).await;

    // Now setting to api_key should succeed
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/active-provider",
        serde_json::json!({ "provider": "api_key" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "api_key with configured key should succeed"
    );

    // Verify via get
    let (_, body) = helpers::get_json(&app, &token, "/api/users/me/active-provider").await;
    assert_eq!(body["provider"], "api_key");
    assert_eq!(body["has_api_key"], true);
}

// ---------------------------------------------------------------------------
// Provider type validation for empty string
// ---------------------------------------------------------------------------

/// Empty `provider_type` is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_provider_empty_type_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmempty", "llmempty@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "",
            "label": "empty type",
            "env_vars": {},
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "empty provider_type should be rejected"
    );
}

// ---------------------------------------------------------------------------
// Model validation
// ---------------------------------------------------------------------------

/// Model field too long is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_provider_model_too_long(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "llmmodel", "llmmodel@test.com").await;

    let long_model = "x".repeat(256);
    let (status, _) = helpers::post_json(
        &app,
        &token,
        "/api/users/me/llm-providers",
        serde_json::json!({
            "provider_type": "bedrock",
            "label": "long model",
            "env_vars": {
                "AWS_ACCESS_KEY_ID": "AKIA7777777777777777",
                "AWS_SECRET_ACCESS_KEY": "secret7777777777777777777777777777777777",
            },
            "model": long_model,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "model too long should be rejected"
    );
}
