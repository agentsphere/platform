mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use std::path::Path;
use uuid::Uuid;

/// Create a project with a git repo and an initial unsigned commit.
async fn setup_project_with_commit(
    app: &axum::Router,
    admin_token: &str,
    state: &platform::store::AppState,
) -> (Uuid, String) {
    // Create a project
    let (status, body) = helpers::post_json(
        app,
        admin_token,
        "/api/projects",
        serde_json::json!({
            "name": "sig-test",
            "description": "Signature test project"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create project: {body}");
    let project_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    // Get the repo path and create a bare repo with a commit
    let repo_dir = state
        .config
        .git_repos_path
        .join("admin")
        .join("sig-test.git");
    std::fs::create_dir_all(&repo_dir).unwrap();

    // Init bare repo
    let output = std::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(&repo_dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git init failed");

    // Create a temp working copy, commit, and push
    let work_dir = tempfile::tempdir().unwrap();
    let work_path = work_dir.path();

    std::process::Command::new("git")
        .arg("clone")
        .arg(&repo_dir)
        .arg(work_path)
        .output()
        .unwrap();

    // Configure git
    std::process::Command::new("git")
        .arg("-C")
        .arg(work_path)
        .args(["config", "user.email", "admin@localhost"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(work_path)
        .args(["config", "user.name", "Admin"])
        .output()
        .unwrap();

    // Create a file and commit
    std::fs::write(work_path.join("README.md"), "# Test\n").unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(work_path)
        .args(["add", "README.md"])
        .output()
        .unwrap();
    let commit_output = std::process::Command::new("git")
        .arg("-C")
        .arg(work_path)
        .args(["commit", "-m", "Initial commit"])
        .output()
        .unwrap();
    assert!(
        commit_output.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit_output.stderr)
    );

    // Get the SHA
    let sha_output = std::process::Command::new("git")
        .arg("-C")
        .arg(work_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let sha = String::from_utf8(sha_output.stdout)
        .unwrap()
        .trim()
        .to_owned();

    // Push to bare repo
    let push_output = std::process::Command::new("git")
        .arg("-C")
        .arg(work_path)
        .args(["push", "origin", "HEAD:refs/heads/main"])
        .output()
        .unwrap();
    assert!(
        push_output.status.success(),
        "git push failed: {}",
        String::from_utf8_lossy(&push_output.stderr)
    );

    (project_id, sha)
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_commits_without_verify_flag_no_signature(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let (project_id, _sha) = setup_project_with_commit(&app, &admin_token, &state).await;

    // Fetch commits without verify_signatures flag
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits?ref=main"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let commits = body.as_array().unwrap();
    assert!(!commits.is_empty());
    // Without verify_signatures, signature field should be absent
    assert!(
        commits[0].get("signature").is_none(),
        "signature should be absent without verify flag"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn test_commits_with_verify_flag_unsigned(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let (project_id, _sha) = setup_project_with_commit(&app, &admin_token, &state).await;

    // Fetch commits WITH verify_signatures flag
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits?ref=main&verify_signatures=true"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let commits = body.as_array().unwrap();
    assert!(!commits.is_empty());
    // Unsigned commit should have NoSignature status
    assert_eq!(commits[0]["signature"]["status"], "no_signature");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_commit_detail_endpoint_unsigned(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let (project_id, sha) = setup_project_with_commit(&app, &admin_token, &state).await;

    // Fetch single commit detail (always verifies signature)
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/{sha}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sha"], sha);
    assert_eq!(body["signature"]["status"], "no_signature");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_commit_detail_nonexistent_sha_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let (project_id, _sha) = setup_project_with_commit(&app, &admin_token, &state).await;

    // Use a nonexistent SHA
    let fake_sha = "0000000000000000000000000000000000000000";
    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/{fake_sha}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_commit_detail_unauthenticated_returns_401(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let (project_id, sha) = setup_project_with_commit(&app, &admin_token, &state).await;

    // Use a bad token
    let (status, _body) = helpers::get_json(
        &app,
        "bad-token",
        &format!("/api/projects/{project_id}/commits/{sha}"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_commit_detail_invalid_sha_returns_400(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let (project_id, _sha) = setup_project_with_commit(&app, &admin_token, &state).await;

    // Use an invalid SHA (too short)
    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/abc"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_signature_cache_hit(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let (project_id, sha) = setup_project_with_commit(&app, &admin_token, &state).await;

    // First call — computes and caches
    let (status, body1) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/{sha}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body1}");
    assert_eq!(body1["signature"]["status"], "no_signature");

    // Verify cache key exists in Valkey
    use fred::interfaces::KeysInterface;
    let cache_key = format!("gpg:sig:{project_id}:{sha}");
    let exists: bool = state.valkey.exists(&cache_key).await.unwrap();
    assert!(exists, "cache key should exist after first verification");

    // Second call — should hit cache
    let (status, body2) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/{sha}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body2}");
    assert_eq!(body2["signature"]["status"], "no_signature");
}

// ---------------------------------------------------------------------------
// GPG-signed commit helpers
// ---------------------------------------------------------------------------

/// Run a command and assert success, returning stdout.
fn run_cmd(program: &str, args: &[&str]) -> String {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program} {}: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "{program} {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Run a command with env vars and assert success.
fn run_cmd_env(program: &str, args: &[&str], env: &[(&str, &str)]) -> String {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{program} {}: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "{program} {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Generate a GPG key pair in a temp GNUPGHOME.
/// Returns (gnupghome, armor_pub_key, fingerprint).
fn generate_gpg_key(email: &str, name: &str) -> (tempfile::TempDir, String, String) {
    let gnupghome = tempfile::tempdir().unwrap();
    let home = gnupghome.path().to_str().unwrap();

    // GPG requires 0700 on homedir
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(gnupghome.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    let batch = format!(
        "%no-protection\nKey-Type: eddsa\nKey-Curve: Ed25519\nKey-Usage: sign\nName-Real: {name}\nName-Email: {email}\nExpire-Date: 0\n%commit\n"
    );
    let batch_file = gnupghome.path().join("keygen.txt");
    std::fs::write(&batch_file, &batch).unwrap();

    run_cmd_env(
        "gpg",
        &[
            "--homedir",
            home,
            "--batch",
            "--pinentry-mode",
            "loopback",
            "--passphrase",
            "",
            "--gen-key",
            batch_file.to_str().unwrap(),
        ],
        &[("GNUPGHOME", home)],
    );

    let armor = run_cmd_env(
        "gpg",
        &["--homedir", home, "--armor", "--export", email],
        &[("GNUPGHOME", home)],
    );

    // Extract the fingerprint for use with user.signingkey
    let list_output = run_cmd_env(
        "gpg",
        &["--homedir", home, "--list-keys", "--with-colons", email],
        &[("GNUPGHOME", home)],
    );
    let fingerprint = list_output
        .lines()
        .find(|l| l.starts_with("fpr:"))
        .and_then(|l| l.split(':').nth(9))
        .unwrap_or("")
        .to_owned();

    (gnupghome, armor, fingerprint)
}

/// Create a project with a bare repo and a GPG-signed commit.
/// `signing_fingerprint` tells git which key to sign with (avoids GPG email mismatch).
/// Returns (project_id, commit_sha).
async fn setup_project_with_signed_commit(
    app: &axum::Router,
    admin_token: &str,
    state: &platform::store::AppState,
    project_name: &str,
    author_email: &str,
    gnupghome: &Path,
    signing_fingerprint: &str,
) -> (Uuid, String) {
    let (status, body) = helpers::post_json(
        app,
        admin_token,
        "/api/projects",
        serde_json::json!({
            "name": project_name,
            "description": "GPG sig test"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create project: {body}");
    let project_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    let repo_dir = state
        .config
        .git_repos_path
        .join("admin")
        .join(format!("{project_name}.git"));
    std::fs::create_dir_all(&repo_dir).unwrap();

    run_cmd("git", &["init", "--bare", repo_dir.to_str().unwrap()]);

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path().to_str().unwrap();
    let home = gnupghome.to_str().unwrap();

    run_cmd("git", &["clone", repo_dir.to_str().unwrap(), work]);
    run_cmd("git", &["-C", work, "config", "user.email", author_email]);
    run_cmd("git", &["-C", work, "config", "user.name", "Test Signer"]);
    run_cmd("git", &["-C", work, "config", "gpg.program", "gpg"]);
    run_cmd("git", &["-C", work, "config", "commit.gpgsign", "true"]);
    run_cmd(
        "git",
        &["-C", work, "config", "user.signingkey", signing_fingerprint],
    );

    std::fs::write(Path::new(work).join("README.md"), "# Signed\n").unwrap();
    run_cmd("git", &["-C", work, "add", "README.md"]);

    run_cmd_env(
        "git",
        &["-C", work, "commit", "-S", "-m", "Signed commit"],
        &[("GNUPGHOME", home)],
    );

    let sha = run_cmd("git", &["-C", work, "rev-parse", "HEAD"])
        .trim()
        .to_owned();

    run_cmd(
        "git",
        &["-C", work, "push", "origin", "HEAD:refs/heads/main"],
    );

    (project_id, sha)
}

// ---------------------------------------------------------------------------
// R6: GPG-signed commit verification integration tests
// ---------------------------------------------------------------------------

/// Verified: register GPG key with matching email → signed commit → verified
#[sqlx::test(migrations = "./migrations")]
async fn test_gpg_signed_commit_verified(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Generate GPG key for admin@localhost (the bootstrap admin's email)
    let (gnupghome, pub_key_armor, fingerprint) =
        generate_gpg_key("admin@localhost", "Admin Signer");

    // Register the GPG key via API
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/gpg-keys",
        serde_json::json!({ "public_key": pub_key_armor }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register GPG key: {body}");

    // Create a project with a signed commit (author email = admin@localhost)
    let (project_id, sha) = setup_project_with_signed_commit(
        &app,
        &admin_token,
        &state,
        "gpg-verified",
        "admin@localhost",
        gnupghome.path(),
        &fingerprint,
    )
    .await;

    // Verify the commit shows as Verified
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/{sha}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["signature"]["status"], "verified",
        "signed commit with matching email should be verified: {body}"
    );
    assert!(body["signature"]["signer_key_id"].is_string());
    assert!(body["signature"]["signer_fingerprint"].is_string());
}

/// UnverifiedSigner: register GPG key, but commit author email differs → unverified_signer
#[sqlx::test(migrations = "./migrations")]
async fn test_gpg_signed_commit_unverified_signer(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Generate GPG key with admin@localhost (matches admin user)
    let (gnupghome, pub_key_armor, fingerprint) =
        generate_gpg_key("admin@localhost", "Admin Signer");

    // Register the GPG key
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/gpg-keys",
        serde_json::json!({ "public_key": pub_key_armor }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register GPG key: {body}");

    // Create a signed commit but with a DIFFERENT author email.
    // Use user.signingkey to force signing with the admin@localhost key.
    let (project_id, sha) = setup_project_with_signed_commit(
        &app,
        &admin_token,
        &state,
        "gpg-unverified",
        "different@example.com", // author email doesn't match key UID
        gnupghome.path(),
        &fingerprint,
    )
    .await;

    // Verify the commit shows as UnverifiedSigner
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/{sha}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["signature"]["status"], "unverified_signer",
        "signed commit with mismatched email should be unverified_signer: {body}"
    );
    assert!(body["signature"]["signer_key_id"].is_string());
    assert!(body["signature"]["signer_fingerprint"].is_string());
}

/// BadSignature: sign commit with unregistered GPG key → bad_signature
#[sqlx::test(migrations = "./migrations")]
async fn test_gpg_signed_commit_bad_signature(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Generate GPG key but do NOT register it
    let (gnupghome, _pub_key_armor, fingerprint) =
        generate_gpg_key("admin@localhost", "Unregistered Signer");

    // Create a signed commit with the unregistered key
    let (project_id, sha) = setup_project_with_signed_commit(
        &app,
        &admin_token,
        &state,
        "gpg-badsig",
        "admin@localhost",
        gnupghome.path(),
        &fingerprint,
    )
    .await;

    // Verify the commit shows as BadSignature (key not in DB)
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/{sha}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["signature"]["status"], "bad_signature",
        "signed commit with unregistered key should be bad_signature: {body}"
    );
}
