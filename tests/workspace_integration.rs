mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Workspace Integration Tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_workspace(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({
            "name": "test-workspace",
            "display_name": "Test Workspace",
            "description": "A test workspace"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].is_string());
    assert_eq!(body["name"], "test-workspace");
    assert_eq!(body["display_name"], "Test Workspace");
    assert_eq!(body["description"], "A test workspace");
}

#[sqlx::test(migrations = "./migrations")]
async fn create_workspace_duplicate_name(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "dupe-ws" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "dupe-ws" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_workspace_invalid_name(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "has spaces" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_workspace(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "get-ws" }),
    )
    .await;
    let ws_id = create_body["id"].as_str().unwrap();

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/api/workspaces/{ws_id}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "get-ws");
}

#[sqlx::test(migrations = "./migrations")]
async fn list_workspaces(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    for i in 0..3 {
        helpers::post_json(
            &app,
            &admin_token,
            "/api/workspaces",
            serde_json::json!({ "name": format!("list-ws-{i}") }),
        )
        .await;
    }

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/workspaces?limit=10&offset=0").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().len() >= 3);
    assert!(body["total"].as_i64().unwrap() >= 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_workspace(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "upd-ws" }),
    )
    .await;
    let ws_id = create_body["id"].as_str().unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}"),
        serde_json::json!({
            "display_name": "Updated WS",
            "description": "Updated description"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["display_name"], "Updated WS");
    assert_eq!(body["description"], "Updated description");
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_workspace(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "del-ws" }),
    )
    .await;
    let ws_id = create_body["id"].as_str().unwrap();

    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/workspaces/{ws_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) =
        helpers::get_json(&app, &admin_token, &format!("/api/workspaces/{ws_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Membership tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn add_and_list_members(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "member-ws" }),
    )
    .await;
    let ws_id = create_body["id"].as_str().unwrap();

    // Owner should already be a member
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/members"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let members = body.as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["role"], "owner");

    // Add another user
    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "ws-member", "member@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/members"),
        serde_json::json!({ "user_id": user_id, "role": "member" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Should now have 2 members
    let (_, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/members"),
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn remove_member(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "rm-ws" }),
    )
    .await;
    let ws_id = create_body["id"].as_str().unwrap();

    let (user_id, _) = helpers::create_user(&app, &admin_token, "rm-member", "rm@test.com").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/members"),
        serde_json::json!({ "user_id": user_id }),
    )
    .await;

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/members/{user_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Should be back to 1 member
    let (_, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/members"),
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn non_member_cannot_view_workspace(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "priv-ws" }),
    )
    .await;
    let ws_id = create_body["id"].as_str().unwrap();

    // Create a user who is NOT a member
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "outsider", "outsider@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _) =
        helpers::get_json(&app, &user_token, &format!("/api/workspaces/{ws_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn member_can_view_workspace(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "viewable-ws" }),
    )
    .await;
    let ws_id = create_body["id"].as_str().unwrap();

    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "ws-viewer", "viewer@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    // Add as member
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/members"),
        serde_json::json!({ "user_id": user_id, "role": "member" }),
    )
    .await;

    // Now should be able to view
    let (status, body) =
        helpers::get_json(&app, &user_token, &format!("/api/workspaces/{ws_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "viewable-ws");
}

// ---------------------------------------------------------------------------
// Workspace → Project permission tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn workspace_member_gets_project_read(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    // Create workspace
    let (_, ws_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "perm-ws" }),
    )
    .await;
    let ws_id = ws_body["id"].as_str().unwrap();

    // Create project in workspace
    let (_, proj_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({
            "name": "ws-project",
            "workspace_id": ws_id,
        }),
    )
    .await;
    let project_id = proj_body["id"].as_str().unwrap();

    // Create user with NO global role — only workspace membership should grant access.
    // (The "viewer" role includes global project:read, which would bypass the workspace check.)
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "ws-projviewer", "pv@test.com").await;

    // Without workspace membership: project should be hidden
    let (status, _) =
        helpers::get_json(&app, &user_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Add user as workspace member
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/members"),
        serde_json::json!({ "user_id": user_id, "role": "member" }),
    )
    .await;

    // Now user should be able to read the project
    let (status, body) =
        helpers::get_json(&app, &user_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "ws-project");
}

#[sqlx::test(migrations = "./migrations")]
async fn workspace_admin_gets_project_write(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    // Create workspace
    let (_, ws_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "admin-ws" }),
    )
    .await;
    let ws_id = ws_body["id"].as_str().unwrap();

    // Create project in workspace
    let (_, proj_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({
            "name": "ws-admin-project",
            "workspace_id": ws_id,
        }),
    )
    .await;
    let project_id = proj_body["id"].as_str().unwrap();

    // Create user, add as workspace admin
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "ws-admin-user", "wa@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/members"),
        serde_json::json!({ "user_id": user_id, "role": "admin" }),
    )
    .await;

    // Admin should be able to update the project
    let (status, body) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}"),
        serde_json::json!({ "description": "Updated by ws admin" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["description"], "Updated by ws admin");
}

#[sqlx::test(migrations = "./migrations")]
async fn workspace_projects_listed(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    // Create workspace
    let (_, ws_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "list-proj-ws" }),
    )
    .await;
    let ws_id = ws_body["id"].as_str().unwrap();

    // Create 2 projects in workspace
    for i in 0..2 {
        helpers::post_json(
            &app,
            &admin_token,
            "/api/projects",
            serde_json::json!({
                "name": format!("wsp-{i}"),
                "workspace_id": ws_id,
            }),
        )
        .await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/projects"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn non_admin_cannot_modify_workspace(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, ws_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "readonly-ws" }),
    )
    .await;
    let ws_id = ws_body["id"].as_str().unwrap();

    // Add user as regular member
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "readonly-user", "ro@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/members"),
        serde_json::json!({ "user_id": user_id, "role": "member" }),
    )
    .await;

    // Member cannot update workspace
    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/workspaces/{ws_id}"),
        serde_json::json!({ "display_name": "Hacked" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
