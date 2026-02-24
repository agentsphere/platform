//! Integration tests for secrets and user provider keys APIs.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

use helpers::{admin_login, assign_role, create_project, create_user, test_router, test_state};

// ---------------------------------------------------------------------------
// Project-scoped secrets
// ---------------------------------------------------------------------------

/// Create + list project secrets — value is NOT returned, only metadata.
/// Uses admin token since secret:write requires admin permissions (developer role only has secret:read).
#[sqlx::test(migrations = "./migrations")]
async fn create_and_list_project_secrets(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let proj_id = create_project(&app, &admin_token, "sec-proj1", "private").await;

    // Create a secret
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({
            "name": "DB_PASSWORD",
            "value": "super-secret-123",
            "scope": "pipeline",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create secret failed: {body}");
    assert!(body["id"].as_str().is_some());
    assert_eq!(body["name"], "DB_PASSWORD");
    // Value should NOT be in the response
    assert!(body.get("value").is_none() || body["value"].is_null());

    // List secrets
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list secrets failed: {body}");
    let secrets = body["items"].as_array().expect("items should be array");
    assert_eq!(secrets.len(), 1);
    assert_eq!(secrets[0]["name"], "DB_PASSWORD");
}

/// Delete project secret.
#[sqlx::test(migrations = "./migrations")]
async fn delete_project_secret(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let proj_id = create_project(&app, &admin_token, "sec-proj2", "private").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "TO_DELETE", "value": "val", "scope": "all" }),
    )
    .await;

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets/TO_DELETE"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify gone
    let (_, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
    )
    .await;
    let secrets = body["items"].as_array().expect("items should be array");
    assert!(secrets.is_empty());
}

/// Invalid scope → 400.
#[sqlx::test(migrations = "./migrations")]
async fn secret_scope_validation(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let proj_id = create_project(&app, &admin_token, "sec-proj3", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "BAD", "value": "val", "scope": "invalid_scope" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Global (admin) secrets
// ---------------------------------------------------------------------------

/// Admin can create and list global secrets.
#[sqlx::test(migrations = "./migrations")]
async fn create_and_list_global_secrets(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/secrets",
        serde_json::json!({
            "name": "GLOBAL_KEY",
            "value": "global-secret",
            "scope": "all",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create global secret: {body}");

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/admin/secrets").await;
    assert_eq!(status, StatusCode::OK);
    let secrets = body["items"].as_array().expect("items should be an array");
    assert!(
        secrets.iter().any(|s| s["name"] == "GLOBAL_KEY"),
        "global secret not found in {body}"
    );
}

/// Non-admin cannot manage global secrets.
#[sqlx::test(migrations = "./migrations")]
async fn non_admin_cannot_manage_global_secrets(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (user_id, token) = create_user(&app, &admin_token, "secdev4", "secdev4@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &token,
        "/api/admin/secrets",
        serde_json::json!({ "name": "HACK", "value": "nope", "scope": "all" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = helpers::get_json(&app, &token, "/api/admin/secrets").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// User provider keys
// ---------------------------------------------------------------------------

/// PUT + GET /api/users/me/provider-keys/{provider} → key_suffix visible.
#[sqlx::test(migrations = "./migrations")]
async fn user_key_set_and_list(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (user_id, token) = create_user(&app, &admin_token, "keyuser1", "keyuser1@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    // Set key
    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/provider-keys/anthropic",
        serde_json::json!({ "api_key": "sk-ant-api03-test-key-1234" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // List keys
    let (status, body) = helpers::get_json(&app, &token, "/api/users/me/provider-keys").await;
    assert_eq!(status, StatusCode::OK);
    let keys: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["provider"], "anthropic");
    assert_eq!(keys[0]["key_suffix"], "...1234");
}

/// Set + DELETE provider key → gone.
#[sqlx::test(migrations = "./migrations")]
async fn user_key_delete(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (user_id, token) = create_user(&app, &admin_token, "keyuser2", "keyuser2@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    helpers::put_json(
        &app,
        &token,
        "/api/users/me/provider-keys/openai",
        serde_json::json!({ "api_key": "sk-openai-test-key-5678" }),
    )
    .await;

    let (status, _) =
        helpers::delete_json(&app, &token, "/api/users/me/provider-keys/openai").await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // List should be empty
    let (_, body) = helpers::get_json(&app, &token, "/api/users/me/provider-keys").await;
    let keys: Vec<serde_json::Value> = serde_json::from_value(body).unwrap();
    assert!(keys.is_empty());
}

/// Short api_key (<10 chars) → 400.
#[sqlx::test(migrations = "./migrations")]
async fn user_key_too_short_rejected(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (user_id, token) = create_user(&app, &admin_token, "keyuser3", "keyuser3@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let (status, _) = helpers::put_json(
        &app,
        &token,
        "/api/users/me/provider-keys/short",
        serde_json::json!({ "api_key": "abc" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Workspace-scoped secrets
// ---------------------------------------------------------------------------

/// Create + list workspace secrets.
#[sqlx::test(migrations = "./migrations")]
async fn create_and_list_workspace_secrets(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    // Get admin user ID
    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = uuid::Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Create a workspace
    let ws = platform::workspace::service::create_workspace(
        &pool,
        admin_id,
        &format!("ws-{}", uuid::Uuid::new_v4()),
        None,
        None,
    )
    .await
    .unwrap();

    // Create a secret
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{}/secrets", ws.id),
        serde_json::json!({
            "name": "WS_SECRET",
            "value": "workspace-secret-val",
            "scope": "pipeline",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create workspace secret: {body}"
    );
    assert_eq!(body["name"], "WS_SECRET");

    // List
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{}/secrets", ws.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let secrets = body["items"].as_array().expect("items should be array");
    assert_eq!(secrets.len(), 1);
    assert_eq!(secrets[0]["name"], "WS_SECRET");
}

/// Delete workspace secret.
#[sqlx::test(migrations = "./migrations")]
async fn delete_workspace_secret(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = uuid::Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let ws = platform::workspace::service::create_workspace(
        &pool,
        admin_id,
        &format!("ws-del-{}", uuid::Uuid::new_v4()),
        None,
        None,
    )
    .await
    .unwrap();

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{}/secrets", ws.id),
        serde_json::json!({ "name": "DEL_ME", "value": "val", "scope": "all" }),
    )
    .await;

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{}/secrets/DEL_ME", ws.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify empty
    let (_, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{}/secrets", ws.id),
    )
    .await;
    let secrets = body["items"].as_array().expect("items should be array");
    assert!(secrets.is_empty());
}

/// Non-admin workspace member cannot create secrets.
#[sqlx::test(migrations = "./migrations")]
async fn workspace_secret_requires_admin(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = uuid::Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let ws = platform::workspace::service::create_workspace(
        &pool,
        admin_id,
        &format!("ws-perm-{}", uuid::Uuid::new_v4()),
        None,
        None,
    )
    .await
    .unwrap();

    // Create a regular member
    let (member_id, member_token) =
        create_user(&app, &admin_token, "wsmember", "wsmember@test.com").await;
    platform::workspace::service::add_member(&pool, ws.id, member_id, "member")
        .await
        .unwrap();

    // Member cannot create secrets (requires admin/owner)
    let (status, _) = helpers::post_json(
        &app,
        &member_token,
        &format!("/api/workspaces/{}/secrets", ws.id),
        serde_json::json!({ "name": "HACK", "value": "nope", "scope": "all" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Non-member cannot list workspace secrets.
#[sqlx::test(migrations = "./migrations")]
async fn workspace_secret_list_requires_membership(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = uuid::Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let ws = platform::workspace::service::create_workspace(
        &pool,
        admin_id,
        &format!("ws-nomem-{}", uuid::Uuid::new_v4()),
        None,
        None,
    )
    .await
    .unwrap();

    let (_outsider_id, outsider_token) =
        create_user(&app, &admin_token, "wsoutsider", "wsoutsider@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &outsider_token,
        &format!("/api/workspaces/{}/secrets", ws.id),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Global secret delete + environment support
// ---------------------------------------------------------------------------

/// Delete a global secret.
#[sqlx::test(migrations = "./migrations")]
async fn delete_global_secret(pool: PgPool) {
    let state = test_state(pool).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/secrets",
        serde_json::json!({ "name": "TO_DELETE_GLOBAL", "value": "v", "scope": "all" }),
    )
    .await;

    let (status, _) =
        helpers::delete_json(&app, &admin_token, "/api/admin/secrets/TO_DELETE_GLOBAL").await;
    assert_eq!(status, StatusCode::OK);

    // Verify not in list
    let (_, body) = helpers::get_json(&app, &admin_token, "/api/admin/secrets").await;
    let secrets = body["items"].as_array().expect("items should be array");
    assert!(!secrets.iter().any(|s| s["name"] == "TO_DELETE_GLOBAL"));
}

/// Create project secret with environment filter.
#[sqlx::test(migrations = "./migrations")]
async fn create_secret_with_environment(pool: PgPool) {
    let state = test_state(pool).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let proj_id = create_project(&app, &admin_token, "sec-env-proj", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({
            "name": "STAGING_DB",
            "value": "staging-secret",
            "scope": "pipeline",
            "environment": "staging",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create secret with env: {body}"
    );

    // List with environment filter
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets?environment=staging"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let secrets = body["items"].as_array().expect("items should be array");
    assert_eq!(secrets.len(), 1);
    assert_eq!(secrets[0]["name"], "STAGING_DB");
}

/// Invalid environment → 400.
#[sqlx::test(migrations = "./migrations")]
async fn create_secret_invalid_environment(pool: PgPool) {
    let state = test_state(pool).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let proj_id = create_project(&app, &admin_token, "sec-badenv", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({
            "name": "BAD_ENV",
            "value": "val",
            "scope": "pipeline",
            "environment": "invalid_env",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// User provider keys
// ---------------------------------------------------------------------------

/// Delete nonexistent key → 404.
#[sqlx::test(migrations = "./migrations")]
async fn user_key_delete_nonexistent(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (user_id, token) = create_user(&app, &admin_token, "keyuser4", "keyuser4@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let (status, _) =
        helpers::delete_json(&app, &token, "/api/users/me/provider-keys/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
