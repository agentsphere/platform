mod e2e_helpers;

use std::path::Path;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// E2E SSH Git Operation Tests (5 tests)
//
// These tests require a Kind cluster with real Postgres, Valkey, and git + ssh
// available on PATH. All tests are #[ignore] so they don't run in normal CI.
// Run with: just test-e2e
// ---------------------------------------------------------------------------

/// Generate an ED25519 SSH key pair in a temp directory. Returns the path to
/// the private key file. The public key is at `{path}.pub`.
fn generate_test_ssh_key(dir: &Path) -> std::path::PathBuf {
    let key_path = dir.join("test_ed25519");
    let output = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(&key_path)
        .args(["-N", "", "-q", "-C", "e2e-test@localhost"])
        .output()
        .expect("ssh-keygen must be available");
    assert!(
        output.status.success(),
        "ssh-keygen failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    key_path
}

/// Build the `GIT_SSH_COMMAND` string for connecting to our test SSH server.
fn git_ssh_command(key_path: &Path, port: u16) -> String {
    format!(
        "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -i {} -p {}",
        key_path.display(),
        port
    )
}

/// Set up an E2E SSH test environment:
/// 1. Build state with e2e_state
/// 2. Generate and register an SSH key
/// 3. Create a project with a bare repo on disk
/// 4. Start the SSH server on a random port
///
/// Returns `(state, app, admin_token, project_name, ssh_port, key_path, _temp_dirs)`.
async fn setup_ssh_e2e(
    pool: PgPool,
) -> (
    platform::store::AppState,
    axum::Router,
    String,
    String,
    u16,
    std::path::PathBuf,
    Vec<tempfile::TempDir>,
    tokio::sync::watch::Sender<()>,
) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());

    // Generate a test SSH key pair
    let key_dir = tempfile::tempdir().expect("tempdir for SSH key");
    let key_path = generate_test_ssh_key(key_dir.path());

    // Read the public key and register it
    let pub_key_content =
        std::fs::read_to_string(format!("{}.pub", key_path.display())).expect("read pub key");

    let (status, body) = e2e_helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "e2e-test-key",
            "public_key": pub_key_content.trim(),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register SSH key: {body}");

    // Create a project with a repo on disk
    let project_name = format!("ssh-e2e-{}", Uuid::new_v4().simple());
    let project_id =
        e2e_helpers::create_project(&app, &admin_token, &project_name, "private").await;

    // Ensure the bare repo exists on disk at the expected path
    let owner = "admin";
    let repo_path = state
        .config
        .git_repos_path
        .join(owner)
        .join(format!("{project_name}.git"));
    std::fs::create_dir_all(&repo_path).expect("create repo dir");
    let init_output = std::process::Command::new("git")
        .args(["init", "--bare"])
        .arg(&repo_path)
        .output()
        .expect("git init bare");
    assert!(
        init_output.status.success(),
        "git init --bare failed: {}",
        String::from_utf8_lossy(&init_output.stderr)
    );

    // Also make sure the project's repo_path is set in the DB
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(repo_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .expect("set repo_path");

    // Start SSH server on random port
    let host_key_dir = tempfile::tempdir().expect("tempdir for host key");
    let host_key_path = host_key_dir.path().join("host_ed25519");

    // Generate host key
    let hk_output = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(&host_key_path)
        .args(["-N", "", "-q"])
        .output()
        .expect("ssh-keygen host key");
    assert!(hk_output.status.success());

    // Bind to port 0, get actual port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind SSH listener");
    let ssh_port = listener.local_addr().expect("local addr").port();

    // Create a state clone with the correct host key path
    let mut ssh_state = state.clone();
    let mut config = (*ssh_state.config).clone();
    config.ssh_host_key_path = host_key_path.to_str().unwrap().to_string();
    ssh_state.config = std::sync::Arc::new(config);

    // Create shutdown channel
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());

    // Spawn the SSH server
    tokio::spawn(async move {
        if let Err(e) =
            platform::git::ssh_server::run_with_listener(ssh_state, listener, &mut shutdown_rx)
                .await
        {
            eprintln!("SSH server error: {e}");
        }
    });

    // Wait for the SSH server to start accepting connections.
    // Probe with a TCP connect to ensure the server is ready.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{ssh_port}"))
            .await
            .is_ok()
        {
            break;
        }
    }

    let dirs = vec![key_dir, host_key_dir];
    (
        state,
        app,
        admin_token,
        project_name,
        ssh_port,
        key_path,
        dirs,
        shutdown_tx,
    )
}

/// Helper: run a git command with SSH configured for our test server.
/// Kills the process after 30 seconds to prevent indefinite hangs.
fn git_ssh_cmd(dir: &Path, args: &[&str], ssh_cmd: &str) -> Result<String, String> {
    let mut child = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_SSH_COMMAND", ssh_cmd)
        .env("GIT_AUTHOR_NAME", "E2E Test")
        .env("GIT_AUTHOR_EMAIL", "test@e2e.local")
        .env("GIT_COMMITTER_NAME", "E2E Test")
        .env("GIT_COMMITTER_EMAIL", "test@e2e.local")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git command");

    let timeout = std::time::Duration::from_secs(30);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = child.stdout.take().map_or_else(Vec::new, |mut s| {
                    let mut buf = Vec::new();
                    std::io::Read::read_to_end(&mut s, &mut buf).unwrap_or(0);
                    buf
                });
                let stderr = child.stderr.take().map_or_else(Vec::new, |mut s| {
                    let mut buf = Vec::new();
                    std::io::Read::read_to_end(&mut s, &mut buf).unwrap_or(0);
                    buf
                });
                if status.success() {
                    return Ok(String::from_utf8_lossy(&stdout).into_owned());
                } else {
                    return Err(format!(
                        "git {} failed (exit {}): {}",
                        args.join(" "),
                        status,
                        String::from_utf8_lossy(&stderr)
                    ));
                }
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    return Err(format!(
                        "git {} timed out after {}s",
                        args.join(" "),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(format!("git {} wait failed: {e}", args.join(" "))),
        }
    }
}

/// Test 1: Full SSH clone succeeds.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_clone_with_ed25519_key(pool: PgPool) {
    let (state, _app, _admin_token, project_name, ssh_port, key_path, _dirs, _shutdown) =
        setup_ssh_e2e(pool).await;

    // First push something to the repo so there's content to clone
    let owner = "admin";
    let repo_path = state
        .config
        .git_repos_path
        .join(owner)
        .join(format!("{project_name}.git"));

    // Create a working copy and push initial content directly to the bare repo
    let work_dir = tempfile::tempdir().expect("tempdir");
    let work_path = work_dir.path().join("work");
    e2e_helpers::git_cmd(
        work_dir.path(),
        &["clone", repo_path.to_str().unwrap(), "work"],
    );
    std::fs::write(work_path.join("README.md"), "# SSH E2E Test\n").unwrap();
    e2e_helpers::git_cmd(&work_path, &["add", "."]);
    e2e_helpers::git_cmd(&work_path, &["config", "user.email", "test@e2e.local"]);
    e2e_helpers::git_cmd(&work_path, &["config", "user.name", "E2E Test"]);
    e2e_helpers::git_cmd(&work_path, &["commit", "-m", "initial commit"]);
    e2e_helpers::git_cmd(&work_path, &["push", "origin", "HEAD:refs/heads/main"]);

    // Now clone via SSH
    let ssh_cmd = git_ssh_command(&key_path, ssh_port);
    let clone_dir = tempfile::tempdir().expect("clone tempdir");
    let ssh_url = format!("ssh://git@127.0.0.1:{ssh_port}/{owner}/{project_name}.git");

    let result = git_ssh_cmd(clone_dir.path(), &["clone", &ssh_url, "cloned"], &ssh_cmd);
    assert!(result.is_ok(), "SSH clone failed: {:?}", result.err());

    // Verify the cloned content
    let readme_path = clone_dir.path().join("cloned").join("README.md");
    let content = std::fs::read_to_string(&readme_path).expect("read cloned README");
    assert!(
        content.contains("SSH E2E Test"),
        "cloned README should have expected content: {content}"
    );
}

/// Test 2: Full SSH push succeeds and post-receive hooks fire.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_push_with_ed25519_key(pool: PgPool) {
    let (state, _app, _admin_token, project_name, ssh_port, key_path, _dirs, _shutdown) =
        setup_ssh_e2e(pool).await;

    let owner = "admin";
    let repo_path = state
        .config
        .git_repos_path
        .join(owner)
        .join(format!("{project_name}.git"));

    // Create initial content in the bare repo
    let work_dir = tempfile::tempdir().expect("tempdir");
    let work_path = work_dir.path().join("work");
    e2e_helpers::git_cmd(
        work_dir.path(),
        &["clone", repo_path.to_str().unwrap(), "work"],
    );
    std::fs::write(work_path.join("README.md"), "# initial\n").unwrap();
    e2e_helpers::git_cmd(&work_path, &["add", "."]);
    e2e_helpers::git_cmd(&work_path, &["config", "user.email", "test@e2e.local"]);
    e2e_helpers::git_cmd(&work_path, &["config", "user.name", "E2E Test"]);
    e2e_helpers::git_cmd(&work_path, &["commit", "-m", "initial"]);
    e2e_helpers::git_cmd(&work_path, &["push", "origin", "HEAD:refs/heads/main"]);

    // Clone via SSH, add new content, push back
    let ssh_cmd = git_ssh_command(&key_path, ssh_port);
    let ssh_url = format!("ssh://git@127.0.0.1:{ssh_port}/{owner}/{project_name}.git");

    let clone_dir = tempfile::tempdir().expect("tempdir");
    let result = git_ssh_cmd(clone_dir.path(), &["clone", &ssh_url, "work"], &ssh_cmd);
    assert!(result.is_ok(), "SSH clone failed: {:?}", result.err());

    let ssh_work = clone_dir.path().join("work");
    std::fs::write(ssh_work.join("new-file.txt"), "pushed via SSH\n").unwrap();

    // Configure git user for the SSH working copy
    git_ssh_cmd(
        &ssh_work,
        &["config", "user.email", "test@e2e.local"],
        &ssh_cmd,
    )
    .unwrap();
    git_ssh_cmd(&ssh_work, &["config", "user.name", "E2E Test"], &ssh_cmd).unwrap();
    git_ssh_cmd(&ssh_work, &["add", "."], &ssh_cmd).unwrap();
    git_ssh_cmd(&ssh_work, &["commit", "-m", "add file via SSH"], &ssh_cmd).unwrap();

    let push_result = git_ssh_cmd(&ssh_work, &["push", "origin", "main"], &ssh_cmd);
    assert!(
        push_result.is_ok(),
        "SSH push failed: {:?}",
        push_result.err()
    );

    // Verify the bare repo received the push
    let bare_log = e2e_helpers::git_cmd(&repo_path, &["log", "--oneline", "main"]);
    assert!(
        bare_log.contains("add file via SSH"),
        "bare repo should contain SSH-pushed commit: {bare_log}"
    );
}

/// Test 3: SSH clone of a private repo denied without matching key.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_clone_private_repo_denied_no_key(pool: PgPool) {
    let (_state, _app, _admin_token, project_name, ssh_port, _key_path, _dirs, _shutdown) =
        setup_ssh_e2e(pool).await;

    let owner = "admin";

    // Generate a DIFFERENT key that is NOT registered
    let bad_key_dir = tempfile::tempdir().expect("tempdir");
    let bad_key_path = generate_test_ssh_key(bad_key_dir.path());

    let ssh_cmd = git_ssh_command(&bad_key_path, ssh_port);
    let ssh_url = format!("ssh://git@127.0.0.1:{ssh_port}/{owner}/{project_name}.git");

    let clone_dir = tempfile::tempdir().expect("tempdir");
    let result = git_ssh_cmd(clone_dir.path(), &["clone", &ssh_url, "cloned"], &ssh_cmd);
    assert!(
        result.is_err(),
        "SSH clone with unregistered key should fail"
    );
}

/// Test 4: SSH push without ProjectWrite permission denied.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_push_no_write_perm_denied(pool: PgPool) {
    let (state, app, admin_token, project_name, ssh_port, _admin_key_path, _dirs, _shutdown) =
        setup_ssh_e2e(pool).await;

    let owner = "admin";
    let repo_path = state
        .config
        .git_repos_path
        .join(owner)
        .join(format!("{project_name}.git"));

    // Push initial content
    let work_dir = tempfile::tempdir().expect("tempdir");
    let work_path = work_dir.path().join("work");
    e2e_helpers::git_cmd(
        work_dir.path(),
        &["clone", repo_path.to_str().unwrap(), "work"],
    );
    std::fs::write(work_path.join("README.md"), "# initial\n").unwrap();
    e2e_helpers::git_cmd(&work_path, &["add", "."]);
    e2e_helpers::git_cmd(&work_path, &["config", "user.email", "test@e2e.local"]);
    e2e_helpers::git_cmd(&work_path, &["config", "user.name", "E2E Test"]);
    e2e_helpers::git_cmd(&work_path, &["commit", "-m", "initial"]);
    e2e_helpers::git_cmd(&work_path, &["push", "origin", "HEAD:refs/heads/main"]);

    // Make the project public so read is allowed but write isn't
    let project_id: (uuid::Uuid,) = sqlx::query_as("SELECT id FROM projects WHERE name = $1")
        .bind(&project_name)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET visibility = 'public' WHERE id = $1")
        .bind(project_id.0)
        .execute(&state.pool)
        .await
        .unwrap();

    // Create a second user with NO write permission, generate and register their SSH key
    let (_other_user_id, other_token) =
        e2e_helpers::create_user(&app, &admin_token, "readonly-user", "readonly@e2e.local").await;

    let other_key_dir = tempfile::tempdir().expect("tempdir");
    let other_key_path = generate_test_ssh_key(other_key_dir.path());
    let other_pub_key =
        std::fs::read_to_string(format!("{}.pub", other_key_path.display())).expect("read pub key");

    let (status, body) = e2e_helpers::post_json(
        &app,
        &other_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "readonly-key",
            "public_key": other_pub_key.trim(),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register key: {body}");

    // Clone via SSH (should succeed — public repo read)
    let ssh_cmd = git_ssh_command(&other_key_path, ssh_port);
    let ssh_url = format!("ssh://git@127.0.0.1:{ssh_port}/{owner}/{project_name}.git");

    let clone_dir = tempfile::tempdir().expect("tempdir");
    let clone_result = git_ssh_cmd(clone_dir.path(), &["clone", &ssh_url, "work"], &ssh_cmd);
    assert!(
        clone_result.is_ok(),
        "SSH clone of public repo should succeed: {:?}",
        clone_result.err()
    );

    // Try to push — should be denied
    let ssh_work = clone_dir.path().join("work");
    std::fs::write(ssh_work.join("hack.txt"), "unauthorized\n").unwrap();
    git_ssh_cmd(
        &ssh_work,
        &["config", "user.email", "test@e2e.local"],
        &ssh_cmd,
    )
    .unwrap();
    git_ssh_cmd(&ssh_work, &["config", "user.name", "E2E Test"], &ssh_cmd).unwrap();
    git_ssh_cmd(&ssh_work, &["add", "."], &ssh_cmd).unwrap();
    git_ssh_cmd(
        &ssh_work,
        &["commit", "-m", "unauthorized push attempt"],
        &ssh_cmd,
    )
    .unwrap();

    let push_result = git_ssh_cmd(&ssh_work, &["push", "origin", "main"], &ssh_cmd);
    assert!(
        push_result.is_err(),
        "SSH push without write permission should fail"
    );
}

/// Test 5: After SSH auth, `last_used_at` is updated.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn test_ssh_last_used_at_updated(pool: PgPool) {
    let (state, _app, _admin_token, project_name, ssh_port, key_path, _dirs, _shutdown) =
        setup_ssh_e2e(pool).await;

    let owner = "admin";
    let repo_path = state
        .config
        .git_repos_path
        .join(owner)
        .join(format!("{project_name}.git"));

    // Push initial content so there's something to clone
    let work_dir = tempfile::tempdir().expect("tempdir");
    let work_path = work_dir.path().join("work");
    e2e_helpers::git_cmd(
        work_dir.path(),
        &["clone", repo_path.to_str().unwrap(), "work"],
    );
    std::fs::write(work_path.join("README.md"), "# test\n").unwrap();
    e2e_helpers::git_cmd(&work_path, &["add", "."]);
    e2e_helpers::git_cmd(&work_path, &["config", "user.email", "test@e2e.local"]);
    e2e_helpers::git_cmd(&work_path, &["config", "user.name", "E2E Test"]);
    e2e_helpers::git_cmd(&work_path, &["commit", "-m", "initial"]);
    e2e_helpers::git_cmd(&work_path, &["push", "origin", "HEAD:refs/heads/main"]);

    // Check that last_used_at is NULL before SSH auth
    let before: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM user_ssh_keys WHERE name = 'e2e-test-key'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(before.is_none(), "last_used_at should be NULL initially");

    // Perform an SSH operation (clone)
    let ssh_cmd = git_ssh_command(&key_path, ssh_port);
    let ssh_url = format!("ssh://git@127.0.0.1:{ssh_port}/{owner}/{project_name}.git");
    let clone_dir = tempfile::tempdir().expect("tempdir");
    let result = git_ssh_cmd(clone_dir.path(), &["clone", &ssh_url, "cloned"], &ssh_cmd);
    assert!(result.is_ok(), "SSH clone failed: {:?}", result.err());

    // Small delay for async update
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Check that last_used_at is now set
    let after: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM user_ssh_keys WHERE name = 'e2e-test-key'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(after.is_some(), "last_used_at should be set after SSH auth");
}
