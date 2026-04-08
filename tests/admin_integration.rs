// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

mod helpers;

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// E3: Admin Integration Tests (14 tests)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_user(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
async fn admin_create_user_duplicate_email(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    helpers::create_user(&app, &admin_token, "emailuser1", "same@test.com").await;

    // Try again with a different name but the same email
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users",
        serde_json::json!({
            "name": "emailuser2",
            "email": "same@test.com",
            "password": "securepassword",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_user_invalid_email(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    helpers::create_user(&app, &admin_token, "listuser1", "list1@test.com").await;
    helpers::create_user(&app, &admin_token, "listuser2", "list2@test.com").await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/users").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 4); // admin + otel-system + 2 users
    assert_eq!(body["items"].as_array().unwrap().len(), 4);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_get_user_by_id(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, _) = helpers::create_user(&app, &admin_token, "getuser", "get@test.com").await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "getuser");
    assert_eq!(body["email"], "get@test.com");
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_update_user(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, _) = helpers::create_user(&app, &admin_token, "deacuser", "deac@test.com").await;

    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify user is inactive
    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["is_active"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn deactivated_user_cannot_login(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "nologin", "nologin@test.com").await;

    // Deactivate
    let (del_status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;
    assert_eq!(del_status, StatusCode::NO_CONTENT);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "tokenrevoke", "tokenrev@test.com").await;

    // Verify token works
    let (status, _) = helpers::get_json(&app, &user_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);

    // Deactivate user (this should revoke sessions)
    let (del_status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;
    assert_eq!(del_status, StatusCode::NO_CONTENT);

    // Token should no longer work
    let (status, _) = helpers::get_json(&app, &user_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn non_admin_cannot_create_user(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "nolist", "nolist@test.com").await;

    let (status, _) = helpers::get_json(&app, &user_token, "/api/users").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_token_for_user(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    helpers::create_user(&app, &admin_token, "audituser", "audit@test.com").await;

    // Query audit_log directly
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log WHERE action = 'user.create'")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(
        row.0, 1,
        "expected exactly one audit_log entry for user.create"
    );
}

// ---------------------------------------------------------------------------
// Role CRUD
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_role(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/roles",
        json!({ "name": "custom-role", "description": "A custom role" }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "custom-role");
    assert_eq!(body["description"], "A custom role");
    assert_eq!(body["is_system"], false);
    assert!(body["id"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_role_duplicate_fails(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/roles",
        json!({ "name": "dupe-role" }),
    )
    .await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/roles",
        json!({ "name": "dupe-role" }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_list_roles(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/admin/roles").await;

    assert_eq!(status, StatusCode::OK);
    let roles = body["items"].as_array().unwrap();
    // Bootstrap seeds 10 system roles (admin, developer, ops, agent, viewer, agent-dev, agent-ops, agent-test, agent-review, agent-manager)
    assert_eq!(roles.len(), 10);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_list_role_permissions(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Get the admin role ID
    let (_, roles) = helpers::get_json(&app, &admin_token, "/api/admin/roles").await;
    let admin_role = roles["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "admin")
        .unwrap();
    let role_id = admin_role["id"].as_str().unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/admin/roles/{role_id}/permissions"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let perms = body["items"].as_array().unwrap();
    assert!(!perms.is_empty(), "admin role should have permissions");
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_set_role_permissions(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Create a custom role
    let (_, role) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/roles",
        json!({ "name": "perm-role" }),
    )
    .await;
    let role_id = role["id"].as_str().unwrap();

    // Set permissions
    let (status, body) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/admin/roles/{role_id}/permissions"),
        json!({ "permissions": ["project:read", "project:write"] }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);

    // Verify
    let (_, perms) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/admin/roles/{role_id}/permissions"),
    )
    .await;
    let perm_names: Vec<&str> = perms["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(perm_names.contains(&"project:read"));
    assert!(perm_names.contains(&"project:write"));
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_set_system_role_permissions_fails(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Get the admin role ID (which is a system role)
    let (_, roles) = helpers::get_json(&app, &admin_token, "/api/admin/roles").await;
    let admin_role = roles["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "admin")
        .unwrap();
    let role_id = admin_role["id"].as_str().unwrap();

    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/admin/roles/{role_id}/permissions"),
        json!({ "permissions": ["project:read"] }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Delegation CRUD
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_delegation(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "delegate-target", "del@test.com").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/delegations",
        json!({
            "delegate_id": user_id.to_string(),
            "permission": "project:read",
            "reason": "testing delegation"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].is_string());
    assert_eq!(body["permission_name"], "project:read");
    assert_eq!(body["reason"], "testing delegation");
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_revoke_delegation(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "revoke-target", "revoke@test.com").await;

    // Create delegation
    let (_, deleg) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/delegations",
        json!({
            "delegate_id": user_id.to_string(),
            "permission": "project:read",
        }),
    )
    .await;
    let deleg_id = deleg["id"].as_str().unwrap();

    // Revoke
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/admin/delegations/{deleg_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);

    // Revoking again should 404
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/admin/delegations/{deleg_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_list_delegations(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "list-deleg", "listdel@test.com").await;

    // Create a delegation
    helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/delegations",
        json!({
            "delegate_id": user_id.to_string(),
            "permission": "project:read",
        }),
    )
    .await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/admin/delegations").await;

    assert_eq!(status, StatusCode::OK);
    let delegations = body["items"].as_array().unwrap();
    assert_eq!(delegations.len(), 1);
}

// ---------------------------------------------------------------------------
// Service account CRUD
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_service_account(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/service-accounts",
        json!({
            "name": "my-bot",
            "email": "bot@test.com",
            "display_name": "My Bot",
            "description": "A test bot",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["user"]["name"], "my-bot");
    assert_eq!(body["user"]["user_type"], "service_account");
    assert!(body["token"].is_null()); // no scopes = no token
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_service_account_with_token(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/service-accounts",
        json!({
            "name": "token-bot",
            "email": "tokenbot@test.com",
            "scopes": ["project:read"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(body["token"].is_object());
    assert!(
        body["token"]["token"]
            .as_str()
            .unwrap()
            .starts_with("plat_")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_list_service_accounts(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Create a service account first
    helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/service-accounts",
        json!({ "name": "list-bot", "email": "listbot@test.com" }),
    )
    .await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/admin/service-accounts").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 2); // otel-system + list-bot
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|i| i["name"] == "list-bot"));
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_deactivate_service_account(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_, sa) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/service-accounts",
        json!({ "name": "deac-bot", "email": "deacbot@test.com" }),
    )
    .await;
    let sa_id = sa["user"]["id"].as_str().unwrap();

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/admin/service-accounts/{sa_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_deactivate_non_service_account_fails(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Create a regular user (not a service account)
    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "regular-user", "regular@test.com").await;

    // Try to deactivate via service account endpoint
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/admin/service-accounts/{user_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_remove_role(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "role-user", "roleuser@test.com").await;

    // Get developer role ID
    let (_, roles) = helpers::get_json(&app, &admin_token, "/api/admin/roles").await;
    let dev_role = roles["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "developer")
        .unwrap();
    let role_id = dev_role["id"].as_str().unwrap();

    // Assign role
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/admin/users/{user_id}/roles"),
        json!({ "role_id": role_id }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Remove role
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/admin/users/{user_id}/roles/{role_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_remove_role_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "norole-user", "norole@test.com").await;

    let fake_role_id = uuid::Uuid::new_v4();
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/admin/users/{user_id}/roles/{fake_role_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Single resource retrieval + update
// ---------------------------------------------------------------------------

/// Get a single role by ID.
#[sqlx::test(migrations = "./migrations")]
async fn admin_get_role_by_id(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Create a custom role
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/roles",
        serde_json::json!({ "name": "get-test-role", "description": "A test role" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let role_id = body["id"].as_str().unwrap();

    // Get it by ID
    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/api/admin/roles/{role_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "get-test-role");
    assert_eq!(body["description"], "A test role");
}

/// Update a custom role's name and description.
#[sqlx::test(migrations = "./migrations")]
async fn admin_update_role(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/roles",
        serde_json::json!({ "name": "upd-role", "description": "original" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let role_id = body["id"].as_str().unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/admin/roles/{role_id}"),
        serde_json::json!({ "name": "upd-role-v2", "description": "updated desc" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update role failed: {body}");
    assert_eq!(body["name"], "upd-role-v2");
    assert_eq!(body["description"], "updated desc");
}

/// Get a single delegation by ID.
#[sqlx::test(migrations = "./migrations")]
async fn admin_get_delegation_by_id(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "deleg-get", "deleg-get@test.com").await;

    // Create a delegation
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/delegations",
        serde_json::json!({
            "delegate_id": user_id.to_string(),
            "permission": "admin:users",
            "expires_in_hours": 24
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create delegation failed: {body}"
    );
    let delegation_id = body["id"].as_str().unwrap();

    // Get it by ID
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/admin/delegations/{delegation_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get delegation failed: {body}");
    assert_eq!(body["id"], delegation_id);
}

/// Get a single service account by ID.
#[sqlx::test(migrations = "./migrations")]
async fn admin_get_service_account_by_id(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Create a service account
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/service-accounts",
        serde_json::json!({
            "name": "sa-get-test",
            "email": "sa-get-test@test.local",
            "display_name": "SA Get Test"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create SA failed: {body}");
    // Response wraps user in ServiceAccountResponse { user: {...}, token: ... }
    let sa_id = body["user"]["id"]
        .as_str()
        .expect("SA response should have user.id");

    // Get it by ID
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/admin/service-accounts/{sa_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get SA failed: {body}");
    assert_eq!(body["name"], "sa-get-test");
}
