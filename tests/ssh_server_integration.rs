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
        token_scopes: None,
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
        token_scopes: None,
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
        token_scopes: None,
    };
    let project = make_resolved_project(project_id, "public");

    let result = check_access_for_user(&state, &git_user, &project, true).await;
    assert!(result.is_err(), "workspace scope mismatch should be denied");
}

// ---------------------------------------------------------------------------
// SSH server lifecycle tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_server_shutdown_graceful(pool: PgPool) {
    let (mut state, _admin_token) = helpers::test_state(pool).await;

    // Use a unique temp dir so parallel tests don't collide
    let tmp = tempfile::tempdir().unwrap();
    let key_path = tmp.path().join("host_key");

    let mut config = (*state.config).clone();
    config.ssh_host_key_path = key_path.to_str().unwrap().to_string();
    state.config = std::sync::Arc::new(config);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let _local_addr = listener.local_addr().unwrap();

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
    let state_clone = state.clone();

    let handle = tokio::spawn(async move {
        platform::git::ssh_server::run_with_listener(state_clone, listener, &mut shutdown_rx).await
    });

    // Give the server a moment to start accepting
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Signal shutdown
    shutdown_tx.send(()).unwrap();

    // The server should exit cleanly within a reasonable timeout
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("server should shut down within 5s")
        .expect("task should not panic");

    assert!(
        result.is_ok(),
        "run_with_listener should return Ok on shutdown"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_server_generates_host_key(pool: PgPool) {
    let (mut state, _admin_token) = helpers::test_state(pool).await;

    // Use a unique temp dir with a non-existent key file
    let tmp = tempfile::tempdir().unwrap();
    let key_path = tmp.path().join("subdir").join("new_host_key");

    // Key should not exist yet
    assert!(
        !key_path.exists(),
        "key should not exist before server start"
    );

    let mut config = (*state.config).clone();
    config.ssh_host_key_path = key_path.to_str().unwrap().to_string();
    state.config = std::sync::Arc::new(config);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
    let state_clone = state.clone();

    let handle = tokio::spawn(async move {
        platform::git::ssh_server::run_with_listener(state_clone, listener, &mut shutdown_rx).await
    });

    // Let the server start (it generates the key during startup)
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify the key file was created on disk
    assert!(key_path.exists(), "host key should have been generated");

    // Also verify the .pub companion file exists (ssh-keygen creates both).
    // ssh-keygen appends ".pub" to the full path, e.g. "new_host_key" → "new_host_key.pub"
    let pub_path = std::path::PathBuf::from(format!("{}.pub", key_path.to_str().unwrap()));
    assert!(
        pub_path.exists(),
        "public key companion file should exist at {pub_path:?}"
    );

    // Clean shutdown
    shutdown_tx.send(()).unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_server_loads_existing_host_key(pool: PgPool) {
    let (mut state, _admin_token) = helpers::test_state(pool).await;

    // Pre-generate an ED25519 key
    let tmp = tempfile::tempdir().unwrap();
    let key_path = tmp.path().join("existing_host_key");

    let output = tokio::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(&key_path)
        .args(["-N", "", "-q"])
        .output()
        .await
        .expect("ssh-keygen should succeed");
    assert!(
        output.status.success(),
        "ssh-keygen failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Record the key file's modification time
    let meta_before = std::fs::metadata(&key_path).unwrap();
    let mtime_before = meta_before.modified().unwrap();

    let mut config = (*state.config).clone();
    config.ssh_host_key_path = key_path.to_str().unwrap().to_string();
    state.config = std::sync::Arc::new(config);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
    let state_clone = state.clone();

    let handle = tokio::spawn(async move {
        platform::git::ssh_server::run_with_listener(state_clone, listener, &mut shutdown_rx).await
    });

    // Let the server start
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify the key file was NOT regenerated (modification time unchanged)
    let meta_after = std::fs::metadata(&key_path).unwrap();
    let mtime_after = meta_after.modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "existing host key should not be regenerated"
    );

    // Clean shutdown
    shutdown_tx.send(()).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("server should shut down within 5s")
        .expect("task should not panic");
    assert!(result.is_ok(), "run_with_listener should return Ok");
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
    assert_eq!(status, StatusCode::NO_CONTENT);

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
