mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// CRUD Tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_global_command(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "test-cmd",
            "prompt_template": "Do $ARGUMENTS now",
            "persistent_session": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["name"], "test-cmd");
    assert!(body["project_id"].is_null());
    assert_eq!(body["persistent_session"], false);
    assert!(body["id"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn create_project_command(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Create a project first
    let (_, proj) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({
            "name": "cmd-test-project",
            "description": "test",
        }),
    )
    .await;
    let project_id = proj["id"].as_str().unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "proj-cmd",
            "project_id": project_id,
            "prompt_template": "Project-scoped: $ARGUMENTS",
            "persistent_session": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["name"], "proj-cmd");
    assert_eq!(body["project_id"], project_id);
    assert_eq!(body["persistent_session"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_command_requires_admin_for_global(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Create a non-admin user
    let (_user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmduser", "cmduser@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/commands",
        serde_json::json!({
            "name": "forbidden-cmd",
            "prompt_template": "nope",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_commands_returns_global(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Create two global commands
    for name in ["list-a", "list-b"] {
        helpers::post_json(
            &app,
            &admin_token,
            "/api/commands",
            serde_json::json!({
                "name": name,
                "prompt_template": "template",
            }),
        )
        .await;
    }

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/commands").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body["total"].as_i64().unwrap() >= 2);
    assert!(!body["items"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn resolve_project_overrides_global(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Create a project
    let (_, proj) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "resolve-proj", "description": "t" }),
    )
    .await;
    let project_id = proj["id"].as_str().unwrap();

    // Create global command
    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "overlap",
            "prompt_template": "GLOBAL: $ARGUMENTS",
        }),
    )
    .await;

    // Create project-scoped command with same name
    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "overlap",
            "project_id": project_id,
            "prompt_template": "PROJECT: $ARGUMENTS",
        }),
    )
    .await;

    // Resolve with project_id — should get project-scoped version
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands/resolve",
        serde_json::json!({
            "input": "/overlap hello",
            "project_id": project_id,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["name"], "overlap");
    assert_eq!(body["prompt"], "PROJECT: hello");

    // Resolve without project_id — should get global version
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands/resolve",
        serde_json::json!({
            "input": "/overlap world",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["prompt"], "GLOBAL: world");
}

#[sqlx::test(migrations = "./migrations")]
async fn resolve_unknown_command_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands/resolve",
        serde_json::json!({ "input": "/nonexistent args" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn duplicate_global_command_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "dupe", "prompt_template": "first" }),
    )
    .await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "dupe", "prompt_template": "second" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn duplicate_project_command_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let (_, proj) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "dupe-proj", "description": "t" }),
    )
    .await;
    let pid = proj["id"].as_str().unwrap();

    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "dupe", "project_id": pid, "prompt_template": "first" }),
    )
    .await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "dupe", "project_id": pid, "prompt_template": "second" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn same_name_different_projects_ok(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let mut pids = Vec::new();
    for name in ["proj-a", "proj-b"] {
        let (_, proj) = helpers::post_json(
            &app,
            &admin_token,
            "/api/projects",
            serde_json::json!({ "name": name, "description": "t" }),
        )
        .await;
        pids.push(proj["id"].as_str().unwrap().to_owned());
    }

    for pid in &pids {
        let (status, _) = helpers::post_json(
            &app,
            &admin_token,
            "/api/commands",
            serde_json::json!({ "name": "shared-name", "project_id": pid, "prompt_template": "t" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_command(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "to-delete", "prompt_template": "t" }),
    )
    .await;
    let id = body["id"].as_str().unwrap();

    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/commands/{id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it's gone
    let (status, _) = helpers::get_json(&app, &admin_token, &format!("/api/commands/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_command(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "to-update",
            "prompt_template": "old template",
            "description": "old desc",
        }),
    )
    .await;
    let id = body["id"].as_str().unwrap();

    let (status, body) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/commands/{id}"),
        serde_json::json!({
            "prompt_template": "new template",
            "description": "new desc",
            "persistent_session": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["description"], "new desc");
    assert_eq!(body["persistent_session"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn command_audit_logged(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Create
    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "audit-cmd", "prompt_template": "t" }),
    )
    .await;
    let id = body["id"].as_str().unwrap();

    // Check audit log for create (async write — poll until visible)
    let count = helpers::wait_for_audit(&state.pool, "command.create", 2000).await;
    assert_eq!(count, 1, "expected create audit entry");

    // Update
    helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/commands/{id}"),
        serde_json::json!({ "description": "updated" }),
    )
    .await;

    let update_count = helpers::wait_for_audit(&state.pool, "command.update", 2000).await;
    assert_eq!(update_count, 1, "expected update audit entry");

    // Delete
    helpers::delete_json(&app, &admin_token, &format!("/api/commands/{id}")).await;

    let delete_count = helpers::wait_for_audit(&state.pool, "command.delete", 2000).await;
    assert_eq!(delete_count, 1, "expected delete audit entry");
}

#[sqlx::test(migrations = "./migrations")]
async fn create_command_requires_auth(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/commands",
        serde_json::json!({ "name": "no-auth", "prompt_template": "t" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_command_invalid_name_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Name with spaces
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "has space", "prompt_template": "t" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Empty name
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "", "prompt_template": "t" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// R8: Missing handler edge-case tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_nonexistent_command_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = uuid::Uuid::new_v4();
    let (status, _) =
        helpers::get_json(&app, &admin_token, &format!("/api/commands/{fake_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_nonexistent_command_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = uuid::Uuid::new_v4();
    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/commands/{fake_id}"),
        serde_json::json!({ "description": "nope" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_nonexistent_command_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = uuid::Uuid::new_v4();
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/commands/{fake_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_command_empty_template_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "empty-tmpl", "prompt_template": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_command_oversized_template_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let big_template = "x".repeat(102_401); // > MAX_TEMPLATE_SIZE (102400)
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "big-tmpl", "prompt_template": big_template }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn resolve_command_missing_slash_returns_400(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands/resolve",
        serde_json::json!({ "input": "no-slash here" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_commands_with_project_includes_global(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Create a project
    let (_, proj) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "list-proj", "description": "t" }),
    )
    .await;
    let project_id = proj["id"].as_str().unwrap();

    // Create a global command
    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({ "name": "global-visible", "prompt_template": "g" }),
    )
    .await;

    // Create a project command
    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "proj-only",
            "project_id": project_id,
            "prompt_template": "p",
        }),
    )
    .await;

    // List with project_id should include both
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/commands?project_id={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let items = body["items"].as_array().unwrap();
    let names: Vec<&str> = items.iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"global-visible"),
        "should include global: {names:?}"
    );
    assert!(
        names.contains(&"proj-only"),
        "should include project: {names:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn create_command_nonexistent_project_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_pid = uuid::Uuid::new_v4();
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "orphan",
            "project_id": fake_pid.to_string(),
            "prompt_template": "t",
        }),
    )
    .await;
    // Admin has no project write on nonexistent project → 404
    assert!(
        status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN,
        "expected 404 or 403 for nonexistent project, got {status}"
    );
}

// ---------------------------------------------------------------------------
// Workspace + project both set — rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_command_both_workspace_and_project_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let (_, proj) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "cmd-both-proj", "description": "t" }),
    )
    .await;
    let project_id = proj["id"].as_str().unwrap();

    let ws_id = uuid::Uuid::new_v4();
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "both-set",
            "project_id": project_id,
            "workspace_id": ws_id.to_string(),
            "prompt_template": "t",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(body["error"].as_str().unwrap().contains("cannot set both"));
}

// ---------------------------------------------------------------------------
// Workspace via main endpoint — rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_command_workspace_via_main_endpoint_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let ws_id = uuid::Uuid::new_v4();
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "ws-via-main",
            "workspace_id": ws_id.to_string(),
            "prompt_template": "t",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("POST /api/workspaces")
    );
}

// ---------------------------------------------------------------------------
// Update template validation
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_command_empty_template_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "upd-tmpl-test",
            "prompt_template": "Valid: $ARGUMENTS",
        }),
    )
    .await;
    let id = body["id"].as_str().unwrap();

    // Update with empty template should be rejected
    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/commands/{id}"),
        serde_json::json!({
            "prompt_template": "",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "empty template in update should be rejected"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn update_command_oversized_template_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "upd-big-tmpl",
            "prompt_template": "Valid: $ARGUMENTS",
        }),
    )
    .await;
    let id = body["id"].as_str().unwrap();

    let big_template = "x".repeat(102_401);
    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/commands/{id}"),
        serde_json::json!({
            "prompt_template": big_template,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "oversized template in update should be rejected"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn update_command_oversized_description_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "upd-big-desc",
            "prompt_template": "Valid: $ARGUMENTS",
        }),
    )
    .await;
    let id = body["id"].as_str().unwrap();

    let big_desc = "x".repeat(10_001);
    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/commands/{id}"),
        serde_json::json!({
            "description": big_desc,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "oversized description in update should be rejected"
    );
}

// ---------------------------------------------------------------------------
// Project-scoped permission: user with project write can create command
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_project_command_requires_project_write(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    // Create a project
    let (_, proj) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "cmd-perm-proj", "description": "t" }),
    )
    .await;
    let project_id = proj["id"].as_str().unwrap();
    let project_uuid = uuid::Uuid::parse_str(project_id).unwrap();

    // Create a user without any role on the project
    let (_user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmd-nowrite", "cmd-nowrite@test.com").await;

    // Should fail (no project write)
    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/commands",
        serde_json::json!({
            "name": "denied-cmd",
            "project_id": project_id,
            "prompt_template": "Do $ARGUMENTS",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "user without project write should not create project command"
    );

    // Grant the user project write
    let (writer_id, writer_token) =
        helpers::create_user(&app, &admin_token, "cmd-writer", "cmd-writer@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        writer_id,
        "developer",
        Some(project_uuid),
        &pool,
    )
    .await;

    // Now should succeed
    let (status, _) = helpers::post_json(
        &app,
        &writer_token,
        "/api/commands",
        serde_json::json!({
            "name": "allowed-cmd",
            "project_id": project_id,
            "prompt_template": "Do $ARGUMENTS",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "user with project write should create project command"
    );
}

// ---------------------------------------------------------------------------
// List commands with project_id includes workspace and global
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_commands_project_includes_workspace_and_global(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    // Create a workspace
    let (_, ws_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "cmd-ws-list" }),
    )
    .await;
    let ws_id = ws_body["id"].as_str().unwrap();

    // Create a project in that workspace
    let (_, proj) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({
            "name": "cmd-list-proj",
            "description": "t",
            "workspace_id": ws_id,
        }),
    )
    .await;
    let project_id = proj["id"].as_str().unwrap();

    // Create a global command
    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "global-for-list",
            "prompt_template": "Global: $ARGUMENTS",
        }),
    )
    .await;

    // Create a workspace command
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/commands"),
        serde_json::json!({
            "name": "ws-for-list",
            "prompt_template": "WS: $ARGUMENTS",
        }),
    )
    .await;

    // Create a project command
    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "proj-for-list",
            "project_id": project_id,
            "prompt_template": "Proj: $ARGUMENTS",
        }),
    )
    .await;

    // List with project_id should include all three scopes
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/commands?project_id={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let items = body["items"].as_array().unwrap();
    let names: Vec<&str> = items.iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"global-for-list"),
        "should include global: {names:?}"
    );
    assert!(
        names.contains(&"ws-for-list"),
        "should include workspace: {names:?}"
    );
    assert!(
        names.contains(&"proj-for-list"),
        "should include project: {names:?}"
    );

    // Project commands should appear first (ordering: project=0, workspace=1, global=2)
    let proj_idx = names.iter().position(|n| *n == "proj-for-list").unwrap();
    let ws_idx = names.iter().position(|n| *n == "ws-for-list").unwrap();
    let global_idx = names.iter().position(|n| *n == "global-for-list").unwrap();
    assert!(
        proj_idx < ws_idx,
        "project commands should appear before workspace: proj={proj_idx}, ws={ws_idx}"
    );
    assert!(
        ws_idx < global_idx,
        "workspace commands should appear before global: ws={ws_idx}, global={global_idx}"
    );
}

// ---------------------------------------------------------------------------
// Workspace commands CRUD via workspace route
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn workspace_command_crud(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    // Create a workspace
    let (_, ws_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "cmd-ws-crud" }),
    )
    .await;
    let ws_id = ws_body["id"].as_str().unwrap();

    // Create a workspace command
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/commands"),
        serde_json::json!({
            "name": "ws-cmd",
            "prompt_template": "Workspace: $ARGUMENTS",
            "description": "A workspace command",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["name"], "ws-cmd");
    assert_eq!(body["workspace_id"], ws_id);
    assert!(body["project_id"].is_null());
    let cmd_id = body["id"].as_str().unwrap();

    // List workspace commands should include it
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/commands"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let names: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"ws-cmd"),
        "should include ws-cmd: {names:?}"
    );

    // Delete the workspace command
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/commands/{cmd_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it's gone from list
    let (_, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/commands"),
    )
    .await;
    let names: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"ws-cmd"),
        "should not include ws-cmd after delete: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Workspace command requires admin membership
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn workspace_command_requires_admin(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    // Create a workspace
    let (_, ws_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "cmd-ws-perm" }),
    )
    .await;
    let ws_id = ws_body["id"].as_str().unwrap();

    // Create a user (not a workspace member)
    let (_user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmd-wsnon", "cmd-wsnon@test.com").await;

    // Non-member should be forbidden
    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/workspaces/{ws_id}/commands"),
        serde_json::json!({
            "name": "denied-ws-cmd",
            "prompt_template": "No: $ARGUMENTS",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// List commands with workspace_id
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_commands_workspace_includes_global(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    // Create a workspace
    let (_, ws_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/workspaces",
        serde_json::json!({ "name": "cmd-ws-list2" }),
    )
    .await;
    let ws_id = ws_body["id"].as_str().unwrap();

    // Create a global command
    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "global-wslist",
            "prompt_template": "G: $ARGUMENTS",
        }),
    )
    .await;

    // Create a workspace command
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/workspaces/{ws_id}/commands"),
        serde_json::json!({
            "name": "ws-wslist",
            "prompt_template": "WS: $ARGUMENTS",
        }),
    )
    .await;

    // List with workspace_id -- should include workspace + global
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/commands?workspace_id={ws_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let names: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"global-wslist"),
        "should include global: {names:?}"
    );
    assert!(
        names.contains(&"ws-wslist"),
        "should include workspace: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Resolved commands endpoint
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolved_commands_returns_merged_set(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Create a project
    let (_, proj) = helpers::post_json(
        &app,
        &admin_token,
        "/api/projects",
        serde_json::json!({ "name": "resolved-proj", "description": "t" }),
    )
    .await;
    let project_id = proj["id"].as_str().unwrap();

    // Create a global command
    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "resolved-global",
            "prompt_template": "Global: $ARGUMENTS",
        }),
    )
    .await;

    // Create a project command that overrides
    helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "resolved-global",
            "project_id": project_id,
            "prompt_template": "Project: $ARGUMENTS",
        }),
    )
    .await;

    // GET /api/commands/resolved?project_id=...
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/commands/resolved?project_id={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let items = body.as_array().unwrap();
    // Should only have one entry for "resolved-global" (project overrides global)
    let matching: Vec<_> = items
        .iter()
        .filter(|i| i["name"].as_str() == Some("resolved-global"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "resolved should deduplicate by name: {items:?}"
    );
    // The scope should indicate it's project-level
    assert_eq!(
        matching[0]["scope"].as_str(),
        Some("project"),
        "resolved command should have project scope"
    );
}

// ---------------------------------------------------------------------------
// Description validation
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_command_oversized_description_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let big_desc = "x".repeat(10_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/commands",
        serde_json::json!({
            "name": "big-desc",
            "prompt_template": "Do $ARGUMENTS",
            "description": big_desc,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
