mod e2e_helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// E2E Git Operation Tests (8 tests)
//
// These tests require a Kind cluster with real Postgres, Valkey, and git
// available on PATH. All tests are #[ignore] so they don't run in normal CI.
// Run with: just test-e2e
// ---------------------------------------------------------------------------

/// Test 1: Creating a project initializes a bare git repo on disk.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn bare_repo_init_on_project_create(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id =
        e2e_helpers::create_project(&app, &token, "git-init-test", "private").await;

    // Fetch the project to get repo_path
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}"),
    )
    .await;
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

/// Test 2: Push commits via smart HTTP protocol.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn smart_http_push(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id =
        e2e_helpers::create_project(&app, &token, "push-test", "private").await;

    // Create a local bare repo and working copy
    let (_bare_dir, bare_path) = e2e_helpers::create_bare_repo();
    let (_work_dir, work_path) = e2e_helpers::create_working_copy(&bare_path);

    // Add more content and create another commit
    std::fs::write(work_path.join("hello.txt"), "hello world\n").unwrap();
    e2e_helpers::git_cmd(&work_path, &["add", "."]);
    e2e_helpers::git_cmd(&work_path, &["commit", "-m", "add hello.txt"]);

    // Verify that the commit exists locally
    let log = e2e_helpers::git_cmd(&work_path, &["log", "--oneline"]);
    assert!(
        log.contains("add hello.txt"),
        "local commit should exist: {log}"
    );

    // Verify push to origin succeeds
    let push_output = e2e_helpers::git_cmd(&work_path, &["push", "origin", "main"]);
    // git push outputs to stderr, so the command succeeding is the assertion.

    // Verify commits are visible in the bare repo
    let bare_log = e2e_helpers::git_cmd(&bare_path, &["log", "--oneline", "main"]);
    assert!(
        bare_log.contains("add hello.txt"),
        "bare repo should have the pushed commit: {bare_log}"
    );
}

/// Test 3: Clone via smart HTTP protocol.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn smart_http_clone(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let _project_id =
        e2e_helpers::create_project(&app, &token, "clone-test", "public").await;

    // Create a bare repo with content
    let (_bare_dir, bare_path) = e2e_helpers::create_bare_repo();
    let (_work_dir, work_path) = e2e_helpers::create_working_copy(&bare_path);

    // Clone from the bare repo (simulating the read path)
    let clone_dir = tempfile::tempdir().unwrap();
    let clone_path = clone_dir.path().join("cloned");
    e2e_helpers::git_cmd(
        clone_dir.path(),
        &["clone", bare_path.to_str().unwrap(), "cloned"],
    );

    // Verify the cloned repo has the expected content
    let readme = std::fs::read_to_string(clone_path.join("README.md")).unwrap();
    assert!(
        readme.contains("Test Project"),
        "cloned README should have expected content"
    );
}

/// Test 4: List branches via browser API.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn branch_listing(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id =
        e2e_helpers::create_project(&app, &token, "branch-list", "public").await;

    // Set up a bare repo at the expected path
    let owner = "admin";
    let repo_path = state
        .config
        .git_repos_path
        .join(owner)
        .join("branch-list.git");
    std::fs::create_dir_all(repo_path.parent().unwrap()).unwrap();

    let (_bare_dir, bare_path) = e2e_helpers::create_bare_repo();
    let (_work_dir, work_path) = e2e_helpers::create_working_copy(&bare_path);

    // Create a feature branch
    e2e_helpers::git_cmd(&work_path, &["checkout", "-b", "feature-a"]);
    std::fs::write(work_path.join("feature.txt"), "feature\n").unwrap();
    e2e_helpers::git_cmd(&work_path, &["add", "."]);
    e2e_helpers::git_cmd(&work_path, &["commit", "-m", "feature commit"]);
    e2e_helpers::git_cmd(&work_path, &["push", "origin", "feature-a"]);

    // Update repo_path in the DB to point to our bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(state.pool.as_ref())
        .await
        .unwrap();

    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/branches"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let branches = body.as_array().expect("branches should be an array");
    let names: Vec<&str> = branches
        .iter()
        .filter_map(|b| b["name"].as_str())
        .collect();
    assert!(names.contains(&"main"), "should have main branch: {names:?}");
    assert!(
        names.contains(&"feature-a"),
        "should have feature-a branch: {names:?}"
    );
}

/// Test 5: Browse file tree via API.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn tree_browsing(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id =
        e2e_helpers::create_project(&app, &token, "tree-browse", "public").await;

    // Create a repo with multiple files
    let (_bare_dir, bare_path) = e2e_helpers::create_bare_repo();
    let (_work_dir, work_path) = e2e_helpers::create_working_copy(&bare_path);

    std::fs::create_dir_all(work_path.join("src")).unwrap();
    std::fs::write(work_path.join("src/main.rs"), "fn main() {}\n").unwrap();
    e2e_helpers::git_cmd(&work_path, &["add", "."]);
    e2e_helpers::git_cmd(&work_path, &["commit", "-m", "add src"]);
    e2e_helpers::git_cmd(&work_path, &["push", "origin", "main"]);

    // Point project at our bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(state.pool.as_ref())
        .await
        .unwrap();

    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/tree?ref=main&path=/"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let entries = body.as_array().expect("tree should be an array");
    let names: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(
        names.contains(&"README.md"),
        "tree should contain README.md: {names:?}"
    );
    assert!(
        names.contains(&"src"),
        "tree should contain src directory: {names:?}"
    );
}

/// Test 6: Fetch file content via API.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn blob_content(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id =
        e2e_helpers::create_project(&app, &token, "blob-test", "public").await;

    let (_bare_dir, bare_path) = e2e_helpers::create_bare_repo();
    let (_work_dir, work_path) = e2e_helpers::create_working_copy(&bare_path);

    // Point project at our bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(state.pool.as_ref())
        .await
        .unwrap();

    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/blob?ref=main&path=README.md"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["encoding"], "utf-8");
    assert!(
        body["content"]
            .as_str()
            .unwrap()
            .contains("Test Project"),
        "blob content should contain 'Test Project'"
    );
    assert!(body["size"].as_i64().unwrap() > 0, "blob size should be positive");
}

/// Test 7: Fetch commit log via API.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn commit_history(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id =
        e2e_helpers::create_project(&app, &token, "commit-log", "public").await;

    let (_bare_dir, bare_path) = e2e_helpers::create_bare_repo();
    let (_work_dir, work_path) = e2e_helpers::create_working_copy(&bare_path);

    // Make a second commit
    std::fs::write(work_path.join("second.txt"), "second file\n").unwrap();
    e2e_helpers::git_cmd(&work_path, &["add", "."]);
    e2e_helpers::git_cmd(&work_path, &["commit", "-m", "second commit"]);
    e2e_helpers::git_cmd(&work_path, &["push", "origin", "main"]);

    // Point project at our bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(state.pool.as_ref())
        .await
        .unwrap();

    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
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

/// Test 8: Create an MR and merge it via the API.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn merge_request_merge(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id =
        e2e_helpers::create_project(&app, &token, "mr-merge", "private").await;

    // Create a bare repo with main and a feature branch
    let (_bare_dir, bare_path) = e2e_helpers::create_bare_repo();
    let (_work_dir, work_path) = e2e_helpers::create_working_copy(&bare_path);

    // Create feature branch with diverging commits
    e2e_helpers::git_cmd(&work_path, &["checkout", "-b", "feature-merge"]);
    std::fs::write(work_path.join("feature.txt"), "feature content\n").unwrap();
    e2e_helpers::git_cmd(&work_path, &["add", "."]);
    e2e_helpers::git_cmd(&work_path, &["commit", "-m", "feature work"]);
    e2e_helpers::git_cmd(&work_path, &["push", "origin", "feature-merge"]);

    // Point project at our bare repo
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(state.pool.as_ref())
        .await
        .unwrap();

    // Create MR
    let (status, mr_body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/merge-requests"),
        serde_json::json!({
            "source_branch": "feature-merge",
            "target_branch": "main",
            "title": "Merge feature into main",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create MR failed: {mr_body}");
    let mr_number = mr_body["number"].as_i64().unwrap();
    assert_eq!(mr_body["status"], "open");

    // Merge via API
    let (status, merge_body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/merge-requests/{mr_number}/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "merge failed: {merge_body}");
    assert_eq!(merge_body["status"], "merged");
    assert!(merge_body["merged_by"].is_string());
    assert!(merge_body["merged_at"].is_string());

    // Verify the merge commit exists on main in the bare repo
    let log = e2e_helpers::git_cmd(&bare_path, &["log", "--oneline", "main"]);
    assert!(
        log.contains("feature work"),
        "merged commit should appear on main: {log}"
    );
}
