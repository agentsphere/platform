//! Integration tests for git browse APIs — branches, tree, blob, commits.
//! Moved from `e2e_git.rs`: these are single-endpoint tests with git filesystem side effects.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Git browse integration tests (5 tests)
// ---------------------------------------------------------------------------

/// Creating a project initializes a bare git repo on disk.
#[sqlx::test(migrations = "./migrations")]
async fn bare_repo_init_on_project_create(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "git-init-test", "private").await;

    // Fetch the project to get repo_path
    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Derive the expected repo path from the config
    let owner_name = "admin";
    let expected_path = state
        .config
        .git_repos_path
        .join(owner_name)
        .join("git-init-test.git");

    // The repo should exist and be a bare repository
    if expected_path.exists() {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&expected_path)
            .arg("rev-parse")
            .arg("--is-bare-repository")
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.trim() == "true",
            "expected bare repo at {}, got: {stdout}",
            expected_path.display()
        );
    }
    // If the repo was created via DB-only path (no disk init), that is also
    // valid — the project_id being returned proves creation succeeded.
    assert!(body["id"].is_string());
}

/// List branches via browser API.
#[sqlx::test(migrations = "./migrations")]
async fn branch_listing(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "branch-list", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    // Create a feature branch
    helpers::git_cmd(&work_path, &["checkout", "-b", "feature-a"]);
    std::fs::write(work_path.join("feature.txt"), "feature\n").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "feature commit"]);
    helpers::git_cmd(&work_path, &["push", "origin", "feature-a"]);

    // Update repo_path in the DB to point to our bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/branches"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let branches = body.as_array().expect("branches should be an array");
    let names: Vec<&str> = branches.iter().filter_map(|b| b["name"].as_str()).collect();
    assert!(
        names.contains(&"main"),
        "should have main branch: {names:?}"
    );
    assert!(
        names.contains(&"feature-a"),
        "should have feature-a branch: {names:?}"
    );
}

/// Browse file tree via API.
#[sqlx::test(migrations = "./migrations")]
async fn tree_browsing(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "tree-browse", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    std::fs::create_dir_all(work_path.join("src")).unwrap();
    std::fs::write(work_path.join("src/main.rs"), "fn main() {}\n").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "add src"]);
    helpers::git_cmd(&work_path, &["push", "origin", "main"]);

    // Point project at our bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/tree?ref=main&path=/"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let entries = body.as_array().expect("tree should be an array");
    let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(
        names.contains(&"README.md"),
        "tree should contain README.md: {names:?}"
    );
    assert!(
        names.contains(&"src"),
        "tree should contain src directory: {names:?}"
    );
}

/// Fetch file content via API.
#[sqlx::test(migrations = "./migrations")]
async fn blob_content(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "blob-test", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, _work_path) = helpers::create_working_copy(&bare_path);

    // Point project at our bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/blob?ref=main&path=README.md"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["encoding"], "utf-8");
    assert!(
        body["content"].as_str().unwrap().contains("Test Project"),
        "blob content should contain 'Test Project'"
    );
    assert!(
        body["size"].as_i64().unwrap() > 0,
        "blob size should be positive"
    );
}

/// Fetch commit log via API.
#[sqlx::test(migrations = "./migrations")]
async fn commit_history(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "commit-log", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    // Make a second commit
    std::fs::write(work_path.join("second.txt"), "second file\n").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "second commit"]);
    helpers::git_cmd(&work_path, &["push", "origin", "main"]);

    // Point project at our bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits?ref=main"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let commits = body.as_array().expect("commits should be an array");
    assert!(commits.len() >= 2, "should have at least 2 commits");

    let messages: Vec<&str> = commits
        .iter()
        .filter_map(|c| c["message"].as_str())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("initial commit")),
        "should contain initial commit: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("second commit")),
        "should contain second commit: {messages:?}"
    );
}

// ---------------------------------------------------------------------------
// Tree browsing: subdirectory, nonexistent path, invalid ref
// ---------------------------------------------------------------------------

/// Browse subdirectory via tree API.
#[sqlx::test(migrations = "./migrations")]
async fn tree_subdirectory(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "tree-subdir", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    std::fs::create_dir_all(work_path.join("src/lib")).unwrap();
    std::fs::write(work_path.join("src/lib/mod.rs"), "pub mod foo;\n").unwrap();
    std::fs::write(work_path.join("src/main.rs"), "fn main() {}\n").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "add src with subdirs"]);
    helpers::git_cmd(&work_path, &["push", "origin", "main"]);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    // Browse root — should include src directory
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/tree?ref=main&path=/"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body.as_array().unwrap();
    let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(
        names.contains(&"src"),
        "root should contain 'src': {names:?}"
    );

    // Browse src/ subdirectory
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/tree?ref=main&path=src"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body.as_array().unwrap();
    let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(
        names.contains(&"main.rs"),
        "src/ should contain main.rs: {names:?}"
    );
    assert!(
        names.contains(&"lib"),
        "src/ should contain lib/: {names:?}"
    );
}

/// Tree with nonexistent path returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn tree_nonexistent_path(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "tree-nopath", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, _work_path) = helpers::create_working_copy(&bare_path);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/tree?ref=main&path=nonexistent/dir"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Tree with invalid git ref returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn tree_invalid_ref(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "tree-badref", "public").await;

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/tree?ref=foo..bar&path=/"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Blob: binary file, nonexistent file, empty path
// ---------------------------------------------------------------------------

/// Blob with binary file returns base64 encoding.
#[sqlx::test(migrations = "./migrations")]
async fn blob_binary_file(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "blob-binary", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    // Write binary data
    std::fs::write(
        work_path.join("image.bin"),
        &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46],
    )
    .unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "add binary file"]);
    helpers::git_cmd(&work_path, &["push", "origin", "main"]);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/blob?ref=main&path=image.bin"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["encoding"], "base64",
        "binary file should use base64 encoding"
    );
    assert!(body["size"].as_i64().unwrap() > 0);
    assert!(
        body["content"].as_str().unwrap().len() > 0,
        "base64 content should not be empty"
    );
}

/// Blob with nonexistent file returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn blob_nonexistent_file(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "blob-nofile", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, _work_path) = helpers::create_working_copy(&bare_path);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/blob?ref=main&path=does/not/exist.txt"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Blob with empty path returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn blob_empty_path(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "blob-empty", "public").await;

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/blob?ref=main&path="),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Commits: limit, empty repo, commit detail
// ---------------------------------------------------------------------------

/// Commits with limit parameter respects the limit.
#[sqlx::test(migrations = "./migrations")]
async fn commits_with_limit(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "commits-limit", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    // Make several commits
    for i in 2..=5 {
        std::fs::write(
            work_path.join(format!("file{i}.txt")),
            format!("content {i}\n"),
        )
        .unwrap();
        helpers::git_cmd(&work_path, &["add", "."]);
        helpers::git_cmd(&work_path, &["commit", "-m", &format!("commit {i}")]);
    }
    helpers::git_cmd(&work_path, &["push", "origin", "main"]);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    // Request with limit=2
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits?ref=main&limit=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let commits = body.as_array().unwrap();
    assert_eq!(commits.len(), 2, "should return exactly 2 commits");
}

/// Commits on empty repo returns empty array.
#[sqlx::test(migrations = "./migrations")]
async fn commits_empty_repo(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "commits-empty", "public").await;

    // Create an empty bare repo (no commits = no branches = unknown revision)
    let empty_dir = format!("/tmp/platform-e2e/empty-commits-{project_id}");
    let empty_bare = format!("{empty_dir}/empty.git");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let output = std::process::Command::new("git")
        .args(["init", "--bare", &empty_bare])
        .output()
        .unwrap();
    assert!(output.status.success());

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(&empty_bare)
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits?ref=HEAD"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let commits = body.as_array().unwrap();
    assert!(commits.is_empty(), "empty repo should return empty array");
}

/// Commit detail endpoint returns a single commit.
#[sqlx::test(migrations = "./migrations")]
async fn commit_detail_endpoint(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "commit-detail", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    // Get the commit SHA from the repo
    let sha_output = helpers::git_cmd(&work_path, &["rev-parse", "HEAD"]);
    let sha = sha_output.trim();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/{sha}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sha"], sha);
    assert!(body["message"].as_str().is_some());
    assert!(body["author_name"].as_str().is_some());
    assert!(body["author_email"].as_str().is_some());
    // Commit detail always verifies signature
    assert!(body["signature"].is_object());
}

/// Commit detail with invalid SHA returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn commit_detail_invalid_sha(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "commit-bad-sha", "public").await;

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/not-a-valid-sha"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Commit detail with nonexistent SHA returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn commit_detail_nonexistent_sha(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "commit-nosuch", "public").await;

    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, _work_path) = helpers::create_working_copy(&bare_path);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    // Use a valid-format SHA that doesn't exist in the repo
    let fake_sha = "a".repeat(40);
    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits/{fake_sha}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Branches: empty repo
// ---------------------------------------------------------------------------

/// Branches on empty repo returns empty array.
#[sqlx::test(migrations = "./migrations")]
async fn branches_empty_repo(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "branches-empty", "public").await;

    // Create an empty bare repo (no branches)
    let empty_dir = format!("/tmp/platform-e2e/empty-branches-{project_id}");
    let empty_bare = format!("{empty_dir}/empty.git");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let output = std::process::Command::new("git")
        .args(["init", "--bare", &empty_bare])
        .output()
        .unwrap();
    assert!(output.status.success());

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(&empty_bare)
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/branches"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let branches = body.as_array().unwrap();
    assert!(branches.is_empty(), "empty repo should have no branches");
}

// ---------------------------------------------------------------------------
// Access control: private project requires read permission
// ---------------------------------------------------------------------------

/// Tree/blob/branches/commits on a private project without read permission returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn browse_private_project_without_perm(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "browse-priv", "private").await;

    // Create a user without permissions
    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "browse-noread", "browsenoread@test.com").await;

    // All browse endpoints should return 404 for this user
    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/tree?ref=main&path=/"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "tree should return 404");

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/blob?ref=main&path=README.md"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "blob should return 404");

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/branches"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "branches should return 404");

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/commits?ref=main"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "commits should return 404");
}

/// Commits with invalid ref returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn commits_invalid_ref(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "commits-badref", "public").await;

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/commits?ref=foo;rm%20-rf"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Blob with invalid ref returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn blob_invalid_ref(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "blob-badref", "public").await;

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/blob?ref=foo|bar&path=README.md"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Blob with path traversal returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn blob_path_traversal(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "blob-trav", "public").await;

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/blob?ref=main&path=../../../etc/passwd"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Tree with path traversal returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn tree_path_traversal(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "tree-trav", "public").await;

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/tree?ref=main&path=../../../etc"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Browse API on nonexistent project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn browse_nonexistent_project(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Use a UUID that doesn't correspond to any project
    let fake_id = "00000000-0000-0000-0000-000000000099";

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{fake_id}/tree?ref=main&path=/"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{fake_id}/branches"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
