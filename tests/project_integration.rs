mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// E4: Project Integration Tests (16 tests)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_project(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "test-project" }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].is_string());
    assert_eq!(body["name"], "test-project");
    assert_eq!(body["visibility"], "private");
}

#[sqlx::test(migrations = "./migrations")]
async fn create_project_with_visibility(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "public-proj", "visibility": "public" }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["visibility"], "public");
}

#[sqlx::test(migrations = "./migrations")]
async fn create_project_invalid_name(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "has spaces" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_project_empty_name(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_project_name_too_long(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let long_name = "a".repeat(256);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": long_name }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_project_duplicate_name(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    helpers::create_project(&app, &admin_token, "dupe-proj", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "dupe-proj" }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_project_by_id(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "getproj", "private").await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/api/projects/{project_id}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "getproj");
}

#[sqlx::test(migrations = "./migrations")]
async fn get_nonexistent_project(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let random_id = Uuid::new_v4();
    let (status, _) =
        helpers::get_json(&app, &admin_token, &format!("/api/projects/{random_id}")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_project(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "updateproj", "private").await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}"),
        serde_json::json!({
            "description": "Updated description",
            "visibility": "public",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["description"], "Updated description");
    assert_eq!(body["visibility"], "public");
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_project_soft_delete(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "delproj", "private").await;

    // Delete (soft)
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // GET should return 404
    let (status, _) =
        helpers::get_json(&app, &admin_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_projects_pagination(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    for i in 0..5 {
        helpers::create_project(&app, &admin_token, &format!("pagproj{i}"), "public").await;
    }

    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/projects?limit=2&offset=0").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert!(body["total"].as_i64().unwrap() >= 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_projects_pagination_offset(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    for i in 0..5 {
        helpers::create_project(&app, &admin_token, &format!("offproj{i}"), "public").await;
    }

    let (_, page1) = helpers::get_json(&app, &admin_token, "/api/projects?limit=2&offset=0").await;
    let (_, page2) = helpers::get_json(&app, &admin_token, "/api/projects?limit=2&offset=2").await;

    let page1_ids: Vec<&str> = page1["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    let page2_ids: Vec<&str> = page2["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();

    // Pages should have different items
    for id in &page2_ids {
        assert!(
            !page1_ids.contains(id),
            "offset pages should have different items"
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn private_project_hidden_from_non_owner(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "secret-proj", "private").await;

    // Create another user
    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "outsider", "outsider@test.com").await;

    // Non-owner GET returns 404 (not 403)
    let (status, _) =
        helpers::get_json(&app, &user_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn public_project_visible_to_all(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "open-proj", "public").await;

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "viewer", "viewer@test.com").await;

    let (status, body) =
        helpers::get_json(&app, &user_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "open-proj");
}

#[sqlx::test(migrations = "./migrations")]
async fn project_owner_has_implicit_access(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    // Give user project:write so they can create projects
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "projowner", "owner@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "my-proj", "private").await;

    // Owner can read their own private project
    let (status, _) =
        helpers::get_json(&app, &user_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Owner can update their own project
    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}"),
        serde_json::json!({ "description": "mine" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_project_requires_permission(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "nodel-proj", "public").await;

    // Create a user with no admin or delete permission
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "nodelete", "nodelete@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _) =
        helpers::delete_json(&app, &user_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
