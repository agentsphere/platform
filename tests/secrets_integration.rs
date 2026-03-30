//! Integration tests for secrets and user provider keys APIs.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use sqlx::Row;

use helpers::{assign_role, create_project, create_user, test_router, test_state};

// ---------------------------------------------------------------------------
// Project-scoped secrets
// ---------------------------------------------------------------------------

/// Create + list project secrets — value is NOT returned, only metadata.
/// Uses admin token since secret:write requires admin permissions (developer role only has secret:read).
#[sqlx::test(migrations = "./migrations")]
async fn create_and_list_project_secrets(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    assert_eq!(status, StatusCode::NO_CONTENT);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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

/// PUT + GET /api/users/me/provider-keys/{provider} → `key_suffix` visible.
#[sqlx::test(migrations = "./migrations")]
async fn user_key_set_and_list(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let keys: Vec<serde_json::Value> = serde_json::from_value(body["items"].clone()).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["provider"], "anthropic");
    assert_eq!(keys[0]["key_suffix"], "...1234");
}

/// Set + DELETE provider key → gone.
#[sqlx::test(migrations = "./migrations")]
async fn user_key_delete(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let keys: Vec<serde_json::Value> = serde_json::from_value(body["items"].clone()).unwrap();
    assert!(keys.is_empty());
}

/// Short `api_key` (<10 chars) → 400.
#[sqlx::test(migrations = "./migrations")]
async fn user_key_too_short_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    assert_eq!(status, StatusCode::NO_CONTENT);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/secrets",
        serde_json::json!({ "name": "TO_DELETE_GLOBAL", "value": "v", "scope": "all" }),
    )
    .await;

    let (status, _) =
        helpers::delete_json(&app, &admin_token, "/api/admin/secrets/TO_DELETE_GLOBAL").await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify not in list
    let (_, body) = helpers::get_json(&app, &admin_token, "/api/admin/secrets").await;
    let secrets = body["items"].as_array().expect("items should be array");
    assert!(!secrets.iter().any(|s| s["name"] == "TO_DELETE_GLOBAL"));
}

/// Create project secret with environment filter.
#[sqlx::test(migrations = "./migrations")]
async fn create_secret_with_environment(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

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

// ---------------------------------------------------------------------------
// Secret requests (ask_for_secret)
// ---------------------------------------------------------------------------

/// Create a secret request → pending, then retrieve it.
#[sqlx::test(migrations = "./migrations")]
async fn create_and_get_secret_request(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-proj1", "private").await;
    let session_id = uuid::Uuid::new_v4();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "API_KEY",
            "description": "An API key for the service",
            "environments": ["production"],
            "session_id": session_id,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create request failed: {body}");
    let request_id = body["id"].as_str().expect("should return id");
    assert_eq!(body["status"], "pending");

    // GET the request
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests/{request_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get request failed: {body}");
    assert_eq!(body["status"], "pending");
}

/// Complete a secret request → stores the secret.
#[sqlx::test(migrations = "./migrations")]
async fn complete_secret_request(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-comp", "private").await;
    let session_id = uuid::Uuid::new_v4();

    // Create request
    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "COMP_SECRET",
            "environments": ["staging"],
            "session_id": session_id,
        }),
    )
    .await;
    let request_id = body["id"].as_str().unwrap();

    // Complete it with a value
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests/{request_id}"),
        serde_json::json!({ "value": "the-actual-secret-value" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete request failed: {body}");
    assert_eq!(body["status"], "completed");

    // Verify the secret was stored
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let secrets = body["items"].as_array().expect("items should be array");
    assert!(
        secrets.iter().any(|s| s["name"] == "COMP_SECRET"),
        "completed secret should appear in list"
    );
}

/// Secret request name validation → 400 for invalid names.
#[sqlx::test(migrations = "./migrations")]
async fn secret_request_validates_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-val", "private").await;
    let session_id = uuid::Uuid::new_v4();

    // Empty name should fail
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "",
            "environments": [],
            "session_id": session_id,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Max 10 pending requests per session.
#[sqlx::test(migrations = "./migrations")]
async fn secret_request_max_per_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-max", "private").await;
    let session_id = uuid::Uuid::new_v4();

    // Create 10 requests
    for i in 0..10 {
        let (status, _) = helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{proj_id}/secret-requests"),
            serde_json::json!({
                "name": format!("SECRET_{i}"),
                "environments": [],
                "session_id": session_id,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "request {i} should succeed");
    }

    // 11th should fail
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "SECRET_OVERFLOW",
            "environments": [],
            "session_id": session_id,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "11th request should be rejected"
    );
}

/// Invalid environment in secret request → 400.
#[sqlx::test(migrations = "./migrations")]
async fn secret_request_invalid_environment(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-badenv", "private").await;
    let session_id = uuid::Uuid::new_v4();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "SOME_SECRET",
            "environments": ["invalid_env"],
            "session_id": session_id,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Scoped secret queries (deploy vs agent)
// ---------------------------------------------------------------------------

/// Secrets with scope='deploy' are returned by `query_scoped_secrets` with deploy scope.
#[sqlx::test(migrations = "./migrations")]
async fn query_scoped_secrets_deploy(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "scope-deploy", "private").await;

    // Create secrets with different scopes
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "STAGING_SECRET", "value": "staging-val", "scope": "staging" }),
    )
    .await;
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "AGENT_SECRET", "value": "agent-val", "scope": "agent" }),
    )
    .await;
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "ALL_SECRET", "value": "all-val", "scope": "all" }),
    )
    .await;

    // Query staging scope
    let master_key = platform::secrets::engine::parse_master_key(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let secrets = platform::secrets::engine::query_scoped_secrets(
        &pool,
        &master_key,
        proj_id,
        &["staging", "all"],
        None,
    )
    .await
    .unwrap();

    let names: Vec<&str> = secrets.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"STAGING_SECRET"),
        "should have staging secret"
    );
    assert!(
        names.contains(&"ALL_SECRET"),
        "should have all-scope secret"
    );
    assert!(
        !names.contains(&"AGENT_SECRET"),
        "should NOT have agent secret"
    );
}

/// Secrets with scope='agent' are returned by `query_scoped_secrets` with agent scope.
#[sqlx::test(migrations = "./migrations")]
async fn query_scoped_secrets_agent(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "scope-agent", "private").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "DEPLOY_ONLY", "value": "dv", "scope": "staging" }),
    )
    .await;
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "AGENT_ONLY", "value": "av", "scope": "agent" }),
    )
    .await;
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "ALL_SCOPE", "value": "allv", "scope": "all" }),
    )
    .await;

    let master_key = platform::secrets::engine::parse_master_key(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let secrets = platform::secrets::engine::query_scoped_secrets(
        &pool,
        &master_key,
        proj_id,
        &["agent", "all"],
        None,
    )
    .await
    .unwrap();

    let names: Vec<&str> = secrets.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"AGENT_ONLY"), "should have agent secret");
    assert!(names.contains(&"ALL_SCOPE"), "should have all-scope secret");
    assert!(
        !names.contains(&"DEPLOY_ONLY"),
        "should NOT have deploy secret"
    );
}

/// `query_scoped_secrets` filters by environment correctly.
#[sqlx::test(migrations = "./migrations")]
async fn query_scoped_secrets_environment_filter(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "scope-env", "private").await;

    // Secret with environment=staging
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "STAGING_SECRET", "value": "sv", "scope": "staging", "environment": "staging" }),
    )
    .await;
    // Secret with no environment (applies to all)
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "GLOBAL_SECRET", "value": "gv", "scope": "staging" }),
    )
    .await;
    // Secret with environment=production
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "PROD_SECRET", "value": "pv", "scope": "staging", "environment": "production" }),
    )
    .await;

    let master_key = platform::secrets::engine::parse_master_key(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();

    // Query for staging → STAGING_SECRET + GLOBAL_SECRET (null env matches)
    let secrets = platform::secrets::engine::query_scoped_secrets(
        &pool,
        &master_key,
        proj_id,
        &["staging", "all"],
        Some("staging"),
    )
    .await
    .unwrap();

    let names: Vec<&str> = secrets.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"STAGING_SECRET"),
        "should have staging secret"
    );
    assert!(
        names.contains(&"GLOBAL_SECRET"),
        "should have global (null-env) secret"
    );
    assert!(
        !names.contains(&"PROD_SECRET"),
        "should NOT have production secret"
    );
}

// ---------------------------------------------------------------------------
// Secret request — auth & edge-case tests (R3, R4, R10, R11, R12)
// ---------------------------------------------------------------------------

/// POST secret-request without token → 401.
#[sqlx::test(migrations = "./migrations")]
async fn secret_request_no_token_returns_401(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-noauth", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        "",
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "MY_KEY",
            "environments": [],
            "session_id": uuid::Uuid::new_v4(),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// GET secret-request without token → 401.
#[sqlx::test(migrations = "./migrations")]
async fn get_secret_request_no_token_returns_401(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let fake_id = uuid::Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        "",
        &format!("/api/projects/{fake_id}/secret-requests/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// POST complete secret-request without token → 401.
#[sqlx::test(migrations = "./migrations")]
async fn complete_secret_request_no_token_returns_401(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let fake_id = uuid::Uuid::new_v4();

    let (status, _) = helpers::post_json(
        &app,
        "",
        &format!("/api/projects/{fake_id}/secret-requests/{fake_id}"),
        serde_json::json!({ "value": "s3cr3t" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// GET nonexistent secret-request → 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_nonexistent_secret_request_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-miss", "private").await;
    let fake_request_id = uuid::Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests/{fake_request_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// POST complete nonexistent secret-request → 404.
#[sqlx::test(migrations = "./migrations")]
async fn complete_nonexistent_secret_request_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-miss2", "private").await;
    let fake_request_id = uuid::Uuid::new_v4();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests/{fake_request_id}"),
        serde_json::json!({ "value": "val" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Complete an already-completed request → 400.
#[sqlx::test(migrations = "./migrations")]
async fn complete_already_completed_request_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-dup", "private").await;
    let session_id = uuid::Uuid::new_v4();

    // Create request
    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "DUP_KEY",
            "environments": [],
            "session_id": session_id,
        }),
    )
    .await;
    let request_id = body["id"].as_str().unwrap();

    // Complete it once
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests/{request_id}"),
        serde_json::json!({ "value": "first-value" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Try completing again → 400
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests/{request_id}"),
        serde_json::json!({ "value": "second-value" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Non-authorized user cannot create secret request.
#[sqlx::test(migrations = "./migrations")]
async fn non_authorized_user_cannot_create_secret_request(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-rbac", "private").await;
    // Create a user with no roles on this project
    let (_user_id, user_token) =
        create_user(&app, &admin_token, "norole-user", "norole@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "NO_ACCESS",
            "environments": [],
            "session_id": uuid::Uuid::new_v4(),
        }),
    )
    .await;
    // No SecretRead permission on private project → 404 (avoids leaking existence)
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Validation: >5 environments → 400.
#[sqlx::test(migrations = "./migrations")]
async fn secret_request_too_many_environments(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-envlim", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "BIG_ENV",
            "environments": ["preview", "staging", "production", "preview", "staging", "production"],
            "session_id": uuid::Uuid::new_v4(),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Validation: empty value on complete → 400.
#[sqlx::test(migrations = "./migrations")]
async fn complete_secret_request_empty_value(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-emptyval", "private").await;
    let session_id = uuid::Uuid::new_v4();

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "EMPTY_VAL",
            "environments": [],
            "session_id": session_id,
        }),
    )
    .await;
    let request_id = body["id"].as_str().unwrap();

    // Empty value → 400 (check_length min=1)
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests/{request_id}"),
        serde_json::json!({ "value": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// DevImageBuilt event handler (R13)
// ---------------------------------------------------------------------------

/// `handle_dev_image_built` updates project's `agent_image`.
#[sqlx::test(migrations = "./migrations")]
async fn dev_image_built_updates_project_agent_image(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let proj_id = create_project(&app, &admin_token, "devimg-proj", "private").await;

    // Verify agent_image is initially NULL
    let row = sqlx::query("SELECT agent_image FROM projects WHERE id = $1")
        .bind(proj_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let agent_image: Option<String> = row.get("agent_image");
    assert!(
        agent_image.is_none(),
        "agent_image should be NULL initially"
    );

    // Simulate a DevImageBuilt event via direct handler call
    let event = platform::store::eventbus::PlatformEvent::DevImageBuilt {
        project_id: proj_id,
        image_ref: "registry.local/devimg-proj/dev:abc123".into(),
        pipeline_id: uuid::Uuid::new_v4(),
    };
    let payload = serde_json::to_string(&event).unwrap();
    platform::store::eventbus::handle_event(&state, &payload)
        .await
        .unwrap();

    // Verify agent_image was updated
    let row = sqlx::query("SELECT agent_image FROM projects WHERE id = $1")
        .bind(proj_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let agent_image: Option<String> = row.get("agent_image");
    assert_eq!(
        agent_image.as_deref(),
        Some("registry.local/devimg-proj/dev:abc123"),
        "agent_image should be updated"
    );
}

// ---------------------------------------------------------------------------
// User provider keys (continued)
// ---------------------------------------------------------------------------

/// Delete nonexistent key → 404.
#[sqlx::test(migrations = "./migrations")]
async fn user_key_delete_nonexistent(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "keyuser4", "keyuser4@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let (status, _) =
        helpers::delete_json(&app, &token, "/api/users/me/provider-keys/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// T13/T14: Secret decryption round-trip via API
// ---------------------------------------------------------------------------

/// Read a project secret returns the decrypted value matching what was stored.
#[sqlx::test(migrations = "./migrations")]
async fn read_project_secret_returns_decrypted_value(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "secret-read-proj", "private").await;

    // Create a secret
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/secrets"),
        serde_json::json!({
            "name": "my-secret",
            "value": "super-secret-value-42",
            "scope": "agent",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Read back — should return decrypted value
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/secrets/my-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "read secret failed: {body}");
    assert_eq!(body["name"], "my-secret");
    assert_eq!(
        body["value"], "super-secret-value-42",
        "decrypted value should match original"
    );
}

/// Reading a nonexistent secret returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn read_project_secret_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "secret-nf-proj", "private").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/secrets/nonexistent"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Reading a secret without permission returns 404 (not 403).
#[sqlx::test(migrations = "./migrations")]
async fn read_project_secret_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "secret-perm-proj", "private").await;

    // Create a secret
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/secrets"),
        serde_json::json!({
            "name": "perm-secret",
            "value": "restricted",
            "scope": "agent",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // User with no project role
    let (_uid, user_token) =
        create_user(&app, &admin_token, "no-secret-perm", "nosecperm@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/secrets/perm-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "should return 404 not 403");
}

// ---------------------------------------------------------------------------
// Secret name/value validation
// ---------------------------------------------------------------------------

/// Secret with empty name is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_secret_empty_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "sec-empty-name", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "", "value": "some-val", "scope": "all" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Secret with empty value is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_secret_empty_value(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "sec-empty-val", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "GOOD_NAME", "value": "", "scope": "all" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Delete nonexistent project secret returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn delete_nonexistent_project_secret(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "sec-del-404", "private").await;

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets/nonexistent"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Delete nonexistent global secret returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn delete_nonexistent_global_secret(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, _) =
        helpers::delete_json(&app, &admin_token, "/api/admin/secrets/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Global secret with invalid scope is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_global_secret_invalid_scope(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/secrets",
        serde_json::json!({ "name": "BAD_SCOPE", "value": "val", "scope": "invalid" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Global secret with empty value is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_global_secret_empty_value(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/secrets",
        serde_json::json!({ "name": "EMPTY_VAL", "value": "", "scope": "all" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Global secret with empty name is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_global_secret_empty_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/secrets",
        serde_json::json!({ "name": "", "value": "val", "scope": "all" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Secret request list with filters
// ---------------------------------------------------------------------------

/// List secret requests with session_id filter.
#[sqlx::test(migrations = "./migrations")]
async fn list_secret_requests_filter_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-filt-sess", "private").await;
    let session_a = uuid::Uuid::new_v4();
    let session_b = uuid::Uuid::new_v4();

    // Create requests for two different sessions
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "KEY_A",
            "environments": [],
            "session_id": session_a,
        }),
    )
    .await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "KEY_B",
            "environments": [],
            "session_id": session_b,
        }),
    )
    .await;

    // Filter by session_a
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests?session_id={session_a}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "KEY_A");
}

/// List secret requests with status filter.
#[sqlx::test(migrations = "./migrations")]
async fn list_secret_requests_filter_status(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-filt-st", "private").await;
    let session_id = uuid::Uuid::new_v4();

    // Create two requests
    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "COMPLETED_KEY",
            "environments": [],
            "session_id": session_id,
        }),
    )
    .await;
    let request_id = body["id"].as_str().unwrap().to_string();

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "PENDING_KEY",
            "environments": [],
            "session_id": session_id,
        }),
    )
    .await;

    // Complete the first request
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests/{request_id}"),
        serde_json::json!({ "value": "secret-val" }),
    )
    .await;

    // Filter by status=pending
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests?status=pending"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "PENDING_KEY");

    // Filter by status=completed
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests?status=completed"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "COMPLETED_KEY");
}

/// List secret requests with pagination.
#[sqlx::test(migrations = "./migrations")]
async fn list_secret_requests_pagination(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-page", "private").await;
    let session_id = uuid::Uuid::new_v4();

    // Create 3 requests
    for i in 0..3 {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{proj_id}/secret-requests"),
            serde_json::json!({
                "name": format!("PAGE_KEY_{i}"),
                "environments": [],
                "session_id": session_id,
            }),
        )
        .await;
    }

    // Page 1: limit=2, offset=0
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests?limit=2&offset=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 3);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    // Page 2: limit=2, offset=2
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests?limit=2&offset=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 3);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

/// Create secret on inactive (soft-deleted) project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn create_secret_inactive_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "sec-inactive", "public").await;

    // Soft-delete the project
    sqlx::query("UPDATE projects SET is_active = false WHERE id = $1")
        .bind(proj_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
        serde_json::json!({ "name": "NEW_SECRET", "value": "val", "scope": "all" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Complete secret request with no environments stores secret with NULL env.
#[sqlx::test(migrations = "./migrations")]
async fn complete_secret_request_no_environments(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-noenv", "private").await;
    let session_id = uuid::Uuid::new_v4();

    // Create request with empty environments
    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "NO_ENV_SECRET",
            "environments": [],
            "session_id": session_id,
        }),
    )
    .await;
    let request_id = body["id"].as_str().unwrap();

    // Complete it
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests/{request_id}"),
        serde_json::json!({ "value": "the-value" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {body}");
    assert_eq!(body["status"], "completed");

    // Verify the secret was stored (list should show it)
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let secrets = body["items"].as_array().expect("items should be array");
    assert!(
        secrets.iter().any(|s| s["name"] == "NO_ENV_SECRET"),
        "secret should be stored even with empty environments"
    );
}

/// Complete secret request with multiple environments stores one secret per env.
#[sqlx::test(migrations = "./migrations")]
async fn complete_secret_request_multiple_environments(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-multi-env", "private").await;
    let session_id = uuid::Uuid::new_v4();

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "MULTI_ENV_SECRET",
            "environments": ["staging", "production"],
            "session_id": session_id,
        }),
    )
    .await;
    let request_id = body["id"].as_str().unwrap();

    // Complete
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests/{request_id}"),
        serde_json::json!({ "value": "multi-env-value" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // List staging secrets
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets?environment=staging"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let secrets = body["items"].as_array().expect("items should be array");
    assert!(
        secrets.iter().any(|s| s["name"] == "MULTI_ENV_SECRET"),
        "should have secret in staging"
    );

    // List production secrets
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secrets?environment=production"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let secrets = body["items"].as_array().expect("items should be array");
    assert!(
        secrets.iter().any(|s| s["name"] == "MULTI_ENV_SECRET"),
        "should have secret in production"
    );
}

/// Workspace secret delete by non-admin member is forbidden.
#[sqlx::test(migrations = "./migrations")]
async fn workspace_secret_delete_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = uuid::Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let ws = platform::workspace::service::create_workspace(
        &pool,
        admin_id,
        &format!("ws-delperm-{}", uuid::Uuid::new_v4()),
        None,
        None,
    )
    .await
    .unwrap();

    // Create a secret as admin
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{}/secrets", ws.id),
        serde_json::json!({ "name": "WS_DEL_SEC", "value": "val", "scope": "all" }),
    )
    .await;

    // Create a regular member
    let (member_id, member_token) =
        create_user(&app, &admin_token, "wsdelmem", "wsdelmem@test.com").await;
    platform::workspace::service::add_member(&pool, ws.id, member_id, "member")
        .await
        .unwrap();

    // Member cannot delete secrets (requires admin/owner)
    let (status, _) = helpers::delete_json(
        &app,
        &member_token,
        &format!("/api/workspaces/{}/secrets/WS_DEL_SEC", ws.id),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Delete nonexistent workspace secret returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn workspace_secret_delete_nonexistent(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = uuid::Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let ws = platform::workspace::service::create_workspace(
        &pool,
        admin_id,
        &format!("ws-del404-{}", uuid::Uuid::new_v4()),
        None,
        None,
    )
    .await
    .unwrap();

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{}/secrets/nonexistent", ws.id),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Workspace secret with invalid scope is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_workspace_secret_invalid_scope(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = uuid::Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let ws = platform::workspace::service::create_workspace(
        &pool,
        admin_id,
        &format!("ws-badscope-{}", uuid::Uuid::new_v4()),
        None,
        None,
    )
    .await
    .unwrap();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{}/secrets", ws.id),
        serde_json::json!({ "name": "BAD", "value": "val", "scope": "not_valid" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Secret description > 500 chars is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn secret_request_description_too_long(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let proj_id = create_project(&app, &admin_token, "secreq-desclong", "private").await;

    let long_desc = "x".repeat(501);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{proj_id}/secret-requests"),
        serde_json::json!({
            "name": "LONG_DESC",
            "description": long_desc,
            "environments": [],
            "session_id": uuid::Uuid::new_v4(),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
