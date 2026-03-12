mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use platform::git::smart_http::{GitUser, ResolvedProject, check_access_for_user};

const TEST_ED25519_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKjB6KC6pSWW2pW828DmK4uouTNB2a0nJQx0qLZW+2++ test@example.com";

// ---------------------------------------------------------------------------
// check_access_for_user tests
// ---------------------------------------------------------------------------

/// Helper to create a project for access tests, returning (`project_id`, `owner_id`).
async fn setup_project(
    state: &platform::store::AppState,
    admin_token: &str,
    app: &axum::Router,
    visibility: &str,
) -> (Uuid, Uuid) {
    // Create project via API
    let (status, body) = helpers::post_json(
        app,
        admin_token,
        "/api/projects",
        serde_json::json!({
            "name": format!("test-{}", Uuid::new_v4().simple()),
            "description": "test project",
            "visibility": visibility,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "project create: {body}");
    let project_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    // Get owner_id from the admin user
    let admin_id = sqlx::query_scalar("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&state.pool)
        .await
        .unwrap();

    (project_id, admin_id)
}

fn make_git_user(user_id: Uuid) -> GitUser {
    GitUser {
        user_id,
        user_name: "test-user".into(),
        ip_addr: None,
        boundary_project_id: None,
        boundary_workspace_id: None,
    }
}

fn make_resolved_project(project_id: Uuid, visibility: &str) -> ResolvedProject {
    ResolvedProject {
        project_id,
        repo_disk_path: std::path::PathBuf::from("/tmp/nonexistent"),
        default_branch: "main".into(),
        visibility: visibility.into(),
        owner_id: Uuid::nil(),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn test_check_access_for_user_public_read_ok(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let (project_id, admin_id) = setup_project(&state, &admin_token, &app, "public").await;

    let git_user = make_git_user(admin_id);
    let project = make_resolved_project(project_id, "public");

    let result = check_access_for_user(&state, &git_user, &project, true).await;
    assert!(result.is_ok(), "public read should succeed");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_check_access_for_user_private_read_with_permission(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let (project_id, admin_id) = setup_project(&state, &admin_token, &app, "private").await;

    // Admin has all permissions
    let git_user = make_git_user(admin_id);
    let project = make_resolved_project(project_id, "private");

    let result = check_access_for_user(&state, &git_user, &project, true).await;
    assert!(
        result.is_ok(),
        "admin read of private project should succeed"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn test_check_access_for_user_private_read_no_permission(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let (project_id, _) = setup_project(&state, &admin_token, &app, "private").await;

    // Create a second user with no role
    let (other_user_id, _) =
        helpers::create_user(&app, &admin_token, "other-user", "other@example.com").await;

    let git_user = make_git_user(other_user_id);
    let project = make_resolved_project(project_id, "private");

    let result = check_access_for_user(&state, &git_user, &project, true).await;
    assert!(
        result.is_err(),
        "should be denied: no permission on private repo"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn test_check_access_for_user_write_with_permission(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let (project_id, admin_id) = setup_project(&state, &admin_token, &app, "private").await;

    let git_user = make_git_user(admin_id);
    let project = make_resolved_project(project_id, "private");

    let result = check_access_for_user(&state, &git_user, &project, false).await;
    assert!(result.is_ok(), "admin write should succeed");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_check_access_for_user_write_no_permission(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let (project_id, _) = setup_project(&state, &admin_token, &app, "public").await;

    let (other_user_id, _) =
        helpers::create_user(&app, &admin_token, "writer-user", "writer@example.com").await;

    let git_user = make_git_user(other_user_id);
    let project = make_resolved_project(project_id, "public");

    let result = check_access_for_user(&state, &git_user, &project, false).await;
    assert!(
        result.is_err(),
        "user without write perm should be denied push"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn test_check_access_for_user_internal_read_any_user(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let (project_id, _) = setup_project(&state, &admin_token, &app, "internal").await;

    // Any authenticated user should be able to read internal repos
    let (other_user_id, _) =
        helpers::create_user(&app, &admin_token, "internal-user", "internal@example.com").await;

    let git_user = make_git_user(other_user_id);
    let project = make_resolved_project(project_id, "internal");

    let result = check_access_for_user(&state, &git_user, &project, true).await;
    assert!(result.is_ok(), "any user should read internal projects");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_check_access_for_user_scope_project_mismatch(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let (project_id, admin_id) = setup_project(&state, &admin_token, &app, "public").await;

    // Token scoped to a DIFFERENT project
    let other_project_id = Uuid::new_v4();
    let git_user = GitUser {
        user_id: admin_id,
        user_name: "admin".into(),
        ip_addr: None,
        boundary_project_id: Some(other_project_id),
        boundary_workspace_id: None,
    };
    let project = make_resolved_project(project_id, "public");

    let result = check_access_for_user(&state, &git_user, &project, true).await;
    assert!(result.is_err(), "project scope mismatch should be denied");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_check_access_for_user_scope_workspace_mismatch(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let (project_id, admin_id) = setup_project(&state, &admin_token, &app, "public").await;

    // Token scoped to a DIFFERENT workspace
    let other_workspace_id = Uuid::new_v4();
    let git_user = GitUser {
        user_id: admin_id,
        user_name: "admin".into(),
        ip_addr: None,
        boundary_project_id: None,
        boundary_workspace_id: Some(other_workspace_id),
    };
    let project = make_resolved_project(project_id, "public");

    let result = check_access_for_user(&state, &git_user, &project, true).await;
    assert!(result.is_err(), "workspace scope mismatch should be denied");
}

// ---------------------------------------------------------------------------
// SSH key fingerprint lookup tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_key_fingerprint_lookup(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Add an SSH key via the API
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "lookup-test",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "add key: {body}");
    let fingerprint = body["fingerprint"].as_str().unwrap();

    // Look up by fingerprint (the query the SSH server uses)
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT k.user_id, u.name FROM user_ssh_keys k JOIN users u ON u.id = k.user_id AND u.is_active = true WHERE k.fingerprint = $1",
    )
    .bind(fingerprint)
    .fetch_optional(&state.pool)
    .await
    .unwrap();

    assert!(row.is_some(), "fingerprint lookup should find the user");
    let (_, user_name) = row.unwrap();
    assert_eq!(user_name, "admin");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_key_inactive_user_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Create another user and add an SSH key
    let (other_user_id, other_token) =
        helpers::create_user(&app, &admin_token, "deactivated-user", "deact@example.com").await;

    let (status, body) = helpers::post_json(
        &app,
        &other_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "deact-key",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "add key: {body}");
    let fingerprint = body["fingerprint"].as_str().unwrap().to_string();

    // Deactivate the user
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{other_user_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Fingerprint lookup should NOT find the deactivated user (is_active = true JOIN)
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT k.user_id FROM user_ssh_keys k JOIN users u ON u.id = k.user_id AND u.is_active = true WHERE k.fingerprint = $1",
    )
    .bind(&fingerprint)
    .fetch_optional(&state.pool)
    .await
    .unwrap();

    assert!(
        row.is_none(),
        "deactivated user should not be found by fingerprint"
    );
}
