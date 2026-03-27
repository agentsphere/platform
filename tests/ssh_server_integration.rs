#![allow(clippy::doc_markdown)]

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

// ---------------------------------------------------------------------------
// resolve_project integration tests
// ---------------------------------------------------------------------------

/// resolve_project returns the correct project for owner/repo.
#[sqlx::test(migrations = "./migrations")]
async fn test_resolve_project_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let (_project_id, _admin_id) = setup_project(&state, &admin_token, &app, "public").await;

    // Look up the admin user's name
    let admin_name: String = sqlx::query_scalar("SELECT name FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // List the project we just created
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, name FROM projects WHERE owner_id = (SELECT id FROM users WHERE name = 'admin') AND is_active = true ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    let (project_id, project_name) = row.unwrap();

    let resolved = platform::git::smart_http::resolve_project(
        &state.pool,
        &state.config,
        &admin_name,
        &project_name,
    )
    .await
    .expect("resolve_project should succeed");
    assert_eq!(resolved.project_id, project_id);
    assert_eq!(resolved.visibility, "public");
}

/// resolve_project returns error for non-existent owner/repo.
#[sqlx::test(migrations = "./migrations")]
async fn test_resolve_project_not_found(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;

    let result = platform::git::smart_http::resolve_project(
        &state.pool,
        &state.config,
        "nonexistent-owner",
        "nonexistent-repo",
    )
    .await;
    assert!(result.is_err(), "should fail for nonexistent project");
}

/// resolve_project returns error for deactivated (soft-deleted) project.
#[sqlx::test(migrations = "./migrations")]
async fn test_resolve_project_soft_deleted(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let (project_id, _admin_id) = setup_project(&state, &admin_token, &app, "public").await;

    let admin_name: String = sqlx::query_scalar("SELECT name FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let project_name: String = sqlx::query_scalar("SELECT name FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    // Soft-delete the project
    sqlx::query("UPDATE projects SET is_active = false WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let result = platform::git::smart_http::resolve_project(
        &state.pool,
        &state.config,
        &admin_name,
        &project_name,
    )
    .await;
    assert!(
        result.is_err(),
        "soft-deleted project should not be resolved"
    );
}

// ---------------------------------------------------------------------------
// SSH key last_used_at update test
// ---------------------------------------------------------------------------

/// After a fingerprint lookup, last_used_at should be set.
#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_key_last_used_at_updated(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "last-used-test",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "add key: {body}");
    let fingerprint = body["fingerprint"].as_str().unwrap().to_string();

    // Initially last_used_at should be NULL
    let before: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM user_ssh_keys WHERE fingerprint = $1")
            .bind(&fingerprint)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(before.is_none(), "last_used_at should be NULL initially");

    // Simulate the SSH server's last_used_at update (fire-and-forget)
    sqlx::query("UPDATE user_ssh_keys SET last_used_at = now() WHERE fingerprint = $1")
        .bind(&fingerprint)
        .execute(&pool)
        .await
        .unwrap();

    // Now last_used_at should be set
    let after: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM user_ssh_keys WHERE fingerprint = $1")
            .bind(&fingerprint)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(after.is_some(), "last_used_at should be set after update");
}

// ---------------------------------------------------------------------------
// Unknown fingerprint test
// ---------------------------------------------------------------------------

/// Fingerprint not in DB returns no match.
#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_key_unknown_fingerprint_returns_none(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;

    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT k.user_id FROM user_ssh_keys k JOIN users u ON u.id = k.user_id AND u.is_active = true WHERE k.fingerprint = $1",
    )
    .bind("SHA256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa=")
    .fetch_optional(&state.pool)
    .await
    .unwrap();

    assert!(
        row.is_none(),
        "unknown fingerprint should return no results"
    );
}

// ---------------------------------------------------------------------------
// SSH server run() with ssh_listen = None
// ---------------------------------------------------------------------------

/// `run()` returns Ok immediately when `ssh_listen` is None.
#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_server_run_no_listen(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    // state.config.ssh_listen is already None from test_state()

    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());

    let result = platform::git::ssh_server::run(state, shutdown_rx).await;
    assert!(
        result.is_ok(),
        "run() with ssh_listen=None should return Ok immediately"
    );
}

// ---------------------------------------------------------------------------
// Host key permission tests
// ---------------------------------------------------------------------------

/// Generated host key should have 0600 permissions on Unix.
#[cfg(unix)]
#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_host_key_permissions(pool: PgPool) {
    use std::os::unix::fs::PermissionsExt;

    let (mut state, _admin_token) = helpers::test_state(pool).await;

    let tmp = tempfile::tempdir().unwrap();
    let key_path = tmp.path().join("perm-test-key");

    let mut config = (*state.config).clone();
    config.ssh_host_key_path = key_path.to_str().unwrap().to_string();
    state.config = std::sync::Arc::new(config);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
    let state_clone = state.clone();

    let handle = tokio::spawn(async move {
        platform::git::ssh_server::run_with_listener(state_clone, listener, &mut shutdown_rx).await
    });

    // Let the server start and generate the key
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert!(key_path.exists(), "host key should have been generated");
    let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "host key should have 0600 permissions, got {:04o}",
        mode & 0o777
    );

    shutdown_tx.send(()).unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

// ---------------------------------------------------------------------------
// Token-scoped SSH user with matching project boundary
// ---------------------------------------------------------------------------

/// Token-scoped to a matching project should allow access.
#[sqlx::test(migrations = "./migrations")]
async fn test_check_access_for_user_scope_project_match(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let (project_id, admin_id) = setup_project(&state, &admin_token, &app, "public").await;

    // Token scoped to the SAME project
    let git_user = GitUser {
        user_id: admin_id,
        user_name: "admin".into(),
        ip_addr: None,
        boundary_project_id: Some(project_id),
        boundary_workspace_id: None,
        token_scopes: None,
    };
    let project = make_resolved_project(project_id, "public");

    let result = check_access_for_user(&state, &git_user, &project, true).await;
    assert!(
        result.is_ok(),
        "project scope match should allow access: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Owner can always read their own project
// ---------------------------------------------------------------------------

/// Owner of a private project should be able to read it, even without explicit roles.
#[sqlx::test(migrations = "./migrations")]
async fn test_check_access_owner_private_read(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    // Create a non-admin user who will own the project
    let (owner_id, owner_token) =
        helpers::create_user(&app, &admin_token, "proj-owner", "projowner@example.com").await;
    helpers::assign_role(&app, &admin_token, owner_id, "developer", None, &pool).await;

    // Create a private project owned by this user
    let project_id = helpers::create_project(&app, &owner_token, "owned-proj", "private").await;

    let git_user = make_git_user(owner_id);
    let project = ResolvedProject {
        project_id,
        repo_disk_path: std::path::PathBuf::from("/tmp/nonexistent"),
        default_branch: "main".into(),
        visibility: "private".into(),
        owner_id,
    };

    let result = check_access_for_user(&state, &git_user, &project, true).await;
    assert!(
        result.is_ok(),
        "owner should be able to read own private project: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// enforce_push_protection integration test
// ---------------------------------------------------------------------------

/// Push to an unprotected branch should succeed.
#[sqlx::test(migrations = "./migrations")]
async fn test_enforce_push_protection_unprotected_branch(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let (project_id, admin_id) = setup_project(&state, &admin_token, &app, "public").await;

    let git_user = make_git_user(admin_id);
    let project = make_resolved_project(project_id, "public");

    // Push to an unprotected branch (no protection rules exist)
    let ref_updates = vec![platform::git::hooks::RefUpdate {
        old_sha: "0000000000000000000000000000000000000000".into(),
        new_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        refname: "refs/heads/feature/my-feature".into(),
    }];

    let result = platform::git::smart_http::enforce_push_protection(
        &state,
        &project,
        &git_user,
        &ref_updates,
    )
    .await;
    assert!(
        result.is_ok(),
        "push to unprotected branch should succeed: {result:?}"
    );
}

/// Push to a tag ref should pass protection (protection only covers branches).
#[sqlx::test(migrations = "./migrations")]
async fn test_enforce_push_protection_tag_ref_passes(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());
    let (project_id, admin_id) = setup_project(&state, &admin_token, &app, "public").await;

    let git_user = make_git_user(admin_id);
    let project = make_resolved_project(project_id, "public");

    let ref_updates = vec![platform::git::hooks::RefUpdate {
        old_sha: "0000000000000000000000000000000000000000".into(),
        new_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        refname: "refs/tags/v1.0.0".into(),
    }];

    let result = platform::git::smart_http::enforce_push_protection(
        &state,
        &project,
        &git_user,
        &ref_updates,
    )
    .await;
    assert!(
        result.is_ok(),
        "push to tag ref should pass protection: {result:?}"
    );
}
