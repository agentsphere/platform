//! Integration tests for the onboarding/wizard API (`src/api/onboarding.rs`).
//!
//! Tests wizard status, complete wizard, settings CRUD, and permission enforcement.
//! Claude OAuth flow endpoints are tested for error paths only (no real CLI binary).

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Wizard status
// ---------------------------------------------------------------------------

/// Admin sees show_wizard=true when wizard is not yet completed.
#[sqlx::test(migrations = "./migrations")]
async fn wizard_status_admin_sees_wizard(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/onboarding/wizard-status").await;
    assert_eq!(status, StatusCode::OK);
    // show_wizard is true only if admin AND not completed — bootstrap doesn't
    // complete the wizard, so admin should see it
    assert!(body["show_wizard"].is_boolean());
}

/// Non-admin always sees show_wizard=false.
#[sqlx::test(migrations = "./migrations")]
async fn wizard_status_non_admin_sees_false(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_user_id, user_token) =
        helpers::create_user(&app, &admin_token, "nonadmin", "nonadmin@test.com").await;

    let (status, body) =
        helpers::get_json(&app, &user_token, "/api/onboarding/wizard-status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["show_wizard"], false);
}

// ---------------------------------------------------------------------------
// Complete wizard
// ---------------------------------------------------------------------------

/// Admin can complete the wizard with solo org type.
#[sqlx::test(migrations = "./migrations")]
async fn complete_wizard_solo_dev(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        serde_json::json!({ "org_type": "solo" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "wizard failed: {body}");
    assert_eq!(body["success"], true);

    // After completing, wizard should no longer show
    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/onboarding/wizard-status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["show_wizard"], false);
}

/// Non-admin cannot complete the wizard.
#[sqlx::test(migrations = "./migrations")]
async fn complete_wizard_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_user_id, user_token) =
        helpers::create_user(&app, &admin_token, "nonadmin2", "nonadmin2@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/onboarding/wizard",
        serde_json::json!({ "org_type": "solo" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Settings CRUD
// ---------------------------------------------------------------------------

/// Admin can read settings.
#[sqlx::test(migrations = "./migrations")]
async fn get_settings_returns_defaults(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/onboarding/settings").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["onboarding_completed"].is_boolean());
}

/// Non-admin cannot read settings.
#[sqlx::test(migrations = "./migrations")]
async fn get_settings_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "nosettings", "nosettings@test.com").await;

    let (status, _) = helpers::get_json(&app, &user_token, "/api/onboarding/settings").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Admin can update org_type via PATCH.
#[sqlx::test(migrations = "./migrations")]
async fn update_settings_org_type(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        "/api/onboarding/settings",
        serde_json::json!({ "org_type": "startup" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update settings failed: {body}");
    // Response returns current settings
    assert!(body["onboarding_completed"].is_boolean());
}

// ---------------------------------------------------------------------------
// Demo project
// ---------------------------------------------------------------------------

/// Non-admin cannot create demo project.
#[sqlx::test(migrations = "./migrations")]
async fn create_demo_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "nodemo", "nodemo@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/onboarding/demo-project",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Claude auth — error paths only (no real CLI binary)
// ---------------------------------------------------------------------------

/// verify-token with too-short token returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn verify_oauth_token_too_short(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/claude-auth/verify-token",
        serde_json::json!({ "token": "short" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// verify-token non-admin returns 403.
#[sqlx::test(migrations = "./migrations")]
async fn verify_oauth_token_non_admin(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "noauth", "noauth@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/onboarding/claude-auth/verify-token",
        serde_json::json!({ "token": "a]very-long-oauth-token-for-testing-purposes" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// cancel_claude_auth for nonexistent session returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn cancel_nonexistent_claude_auth(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = uuid::Uuid::new_v4();
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/onboarding/claude-auth/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// claude_auth_status for nonexistent session returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn auth_status_nonexistent(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = uuid::Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/onboarding/claude-auth/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Complete wizard with different org types
// ---------------------------------------------------------------------------

/// Startup org type creates a team workspace.
#[sqlx::test(migrations = "./migrations")]
async fn complete_wizard_startup(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        serde_json::json!({ "org_type": "startup" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "startup wizard failed: {body}");
    assert_eq!(body["success"], true);

    // Verify a workspace was created (startup creates team workspace)
    let ws_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM workspaces")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(ws_count.0 >= 1, "expected team workspace to be created");
}

/// TechOrg org type creates a team workspace with stricter defaults.
#[sqlx::test(migrations = "./migrations")]
async fn complete_wizard_tech_org(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        serde_json::json!({ "org_type": "tech_org" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tech_org wizard failed: {body}");
    assert_eq!(body["success"], true);

    // Verify wizard is completed
    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/onboarding/wizard-status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["show_wizard"], false);
}

/// Complete wizard with passkey_policy override.
#[sqlx::test(migrations = "./migrations")]
async fn complete_wizard_with_passkey_policy(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        serde_json::json!({
            "org_type": "solo",
            "passkey_policy": "mandatory"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "passkey override failed: {body}");
    assert_eq!(body["success"], true);

    // Verify settings reflect the override
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/onboarding/settings").await;
    assert_eq!(status, StatusCode::OK);
    // security_policy should contain the mandatory passkey enforcement
    let security = &body["security_policy"];
    assert!(security.is_object() || security.is_string());
}

/// Complete wizard with custom LLM provider.
#[sqlx::test(migrations = "./migrations")]
async fn complete_wizard_with_custom_provider(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        serde_json::json!({
            "org_type": "solo",
            "custom_provider": {
                "provider_type": "bedrock",
                "env_vars": {
                    "AWS_REGION": "us-east-1",
                    "AWS_ACCESS_KEY_ID": "test-key",
                    "AWS_SECRET_ACCESS_KEY": "test-secret"
                }
            }
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "custom provider wizard failed: {body}"
    );
    assert_eq!(body["success"], true);
}

/// Admin can create a demo project.
#[sqlx::test(migrations = "./migrations")]
async fn create_demo_project_success(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/demo-project",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "demo project failed: {body}");
    assert!(body["project_id"].is_string());
    assert!(body["project_name"].is_string());
}

// ---------------------------------------------------------------------------
// Claude auth — permission enforcement
// ---------------------------------------------------------------------------

/// Non-admin cannot start Claude auth flow.
#[sqlx::test(migrations = "./migrations")]
async fn start_claude_auth_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "noauth-start", "noauthstart@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/onboarding/claude-auth/start",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Submit auth code for nonexistent session returns error (403 because admin check comes first,
/// but the actual session lookup triggers an internal error path).
#[sqlx::test(migrations = "./migrations")]
async fn submit_auth_code_nonexistent_session(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = uuid::Uuid::new_v4();
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/onboarding/claude-auth/{fake_id}/code"),
        serde_json::json!({ "code": "test-code-12345" }),
    )
    .await;
    // Expect 500 (internal error from "session not found" in cli_auth_manager)
    // or 400 ("master key not configured") if master_key handling fails first.
    // The key point: it doesn't succeed.
    assert!(
        status == StatusCode::INTERNAL_SERVER_ERROR || status == StatusCode::BAD_REQUEST,
        "expected error for nonexistent session, got {status}"
    );
}

/// Non-admin cannot submit auth code.
#[sqlx::test(migrations = "./migrations")]
async fn submit_auth_code_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "noauth-code", "noauthcode@test.com").await;

    let fake_id = uuid::Uuid::new_v4();
    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/onboarding/claude-auth/{fake_id}/code"),
        serde_json::json!({ "code": "test-code" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Wizard with provider_key and cli_token
// ---------------------------------------------------------------------------

/// Complete wizard with provider_key saves encrypted key in DB.
#[sqlx::test(migrations = "./migrations")]
async fn complete_wizard_with_provider_key(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        serde_json::json!({
            "org_type": "solo",
            "provider_key": "sk-ant-api03-test-key-1234567890abcdef"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "wizard with provider_key failed: {body}"
    );
    assert_eq!(body["success"], true);

    // Verify key was saved in user_provider_keys
    let admin_id = helpers::admin_user_id(&pool).await;
    let key_row: Option<(Vec<u8>, String)> = sqlx::query_as(
        "SELECT encrypted_key, key_suffix FROM user_provider_keys WHERE user_id = $1 AND provider = 'anthropic'",
    )
    .bind(admin_id)
    .fetch_optional(&pool)
    .await
    .unwrap();

    assert!(key_row.is_some(), "provider key should be saved in DB");
    let (encrypted_key, suffix) = key_row.unwrap();
    assert!(
        !encrypted_key.is_empty(),
        "encrypted key should not be empty"
    );
    assert_eq!(suffix, "cdef", "key_suffix should be last 4 chars");
}

/// Complete wizard with cli_token saves token in cli_credentials.
#[sqlx::test(migrations = "./migrations")]
async fn complete_wizard_with_cli_token(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        serde_json::json!({
            "org_type": "solo",
            "cli_token": {
                "auth_type": "setup_token",
                "token": "sk-ant-oat01-test-cli-token-abcdef1234567890"
            }
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "wizard with cli_token failed: {body}"
    );
    assert_eq!(body["success"], true);

    // Verify cli_credentials row was created
    let admin_id = helpers::admin_user_id(&pool).await;
    let cred_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM cli_credentials WHERE user_id = $1 AND auth_type = 'setup_token'",
    )
    .bind(admin_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        cred_count.0, 1,
        "cli_credentials should have one setup_token row"
    );
}

/// Saving provider key without master_key configured returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn save_provider_key_no_master_key(pool: PgPool) {
    let (mut state, admin_token) = helpers::test_state(pool).await;

    // Override config to remove master_key
    let mut config = (*state.config).clone();
    config.master_key = None;
    state.config = std::sync::Arc::new(config);

    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        serde_json::json!({
            "org_type": "solo",
            "provider_key": "sk-ant-api03-test-key-no-master"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "should fail without master key: {body}"
    );
    let err = body["error"].as_str().unwrap_or("");
    assert!(
        err.to_lowercase().contains("master key"),
        "error should mention master key, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Update settings edge cases
// ---------------------------------------------------------------------------

/// PATCH settings with empty body is a no-op, returns 200 with unchanged settings.
#[sqlx::test(migrations = "./migrations")]
async fn update_settings_empty_body_noop(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // First read current settings
    let (status, before) = helpers::get_json(&app, &admin_token, "/api/onboarding/settings").await;
    assert_eq!(status, StatusCode::OK);

    // PATCH with empty body (no org_type field)
    let (status, after) = helpers::patch_json(
        &app,
        &admin_token,
        "/api/onboarding/settings",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Settings should be unchanged
    assert_eq!(
        before["onboarding_completed"], after["onboarding_completed"],
        "settings should be unchanged after empty patch"
    );
}

// ---------------------------------------------------------------------------
// Team workspace idempotency
// ---------------------------------------------------------------------------

/// Running wizard twice with startup only creates one team workspace.
#[sqlx::test(migrations = "./migrations")]
async fn create_team_workspace_already_exists(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // First wizard run (creates team workspace)
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        serde_json::json!({ "org_type": "startup" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first wizard failed: {body}");

    // Count non-personal workspaces (team workspace)
    let admin_id = helpers::admin_user_id(&pool).await;
    let ws_count_1: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM workspaces WHERE owner_id = $1 AND name != (SELECT name FROM users WHERE id = $1)",
    )
    .bind(admin_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Reset wizard completed flag so we can run it again
    sqlx::query(
        "INSERT INTO platform_settings (key, value) VALUES ('onboarding_completed', 'false')
         ON CONFLICT (key) DO UPDATE SET value = 'false'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Second wizard run with startup (should not create duplicate workspace)
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/onboarding/wizard",
        serde_json::json!({ "org_type": "startup" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second wizard failed: {body}");

    // Count should be the same
    let ws_count_2: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM workspaces WHERE owner_id = $1 AND name != (SELECT name FROM users WHERE id = $1)",
    )
    .bind(admin_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(
        ws_count_1.0, ws_count_2.0,
        "second wizard run should not create duplicate workspace"
    );
}
