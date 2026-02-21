mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// E6: Issue/MR Integration Tests (15 tests)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_issue(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "issue-proj", "public").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({
            "title": "First issue",
            "body": "This is the body",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["number"], 1);
    assert_eq!(body["title"], "First issue");
    assert_eq!(body["status"], "open");
}

#[sqlx::test(migrations = "./migrations")]
async fn create_issue_empty_title(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "empty-title", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_issue_title_too_long(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "long-title", "public").await;

    let long_title = "a".repeat(501);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": long_title }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_issues(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "list-issues", "public").await;

    for i in 1..=3 {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/issues"),
            serde_json::json!({ "title": format!("Issue {i}") }),
        )
        .await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().len() >= 3);
    assert!(body["total"].as_i64().unwrap() >= 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_issue_by_number(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "get-issue", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "Test Issue" }),
    )
    .await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["number"], 1);
    assert_eq!(body["title"], "Test Issue");
}

#[sqlx::test(migrations = "./migrations")]
async fn update_issue(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "upd-issue", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "Original Title" }),
    )
    .await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
        serde_json::json!({
            "title": "Updated Title",
            "labels": ["bug", "critical"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Updated Title");
    let labels = body["labels"].as_array().unwrap();
    assert!(labels.iter().any(|l| l == "bug"));
    assert!(labels.iter().any(|l| l == "critical"));
}

#[sqlx::test(migrations = "./migrations")]
async fn close_and_reopen_issue(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "close-reopen", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "Close Me" }),
    )
    .await;

    // Close
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
        serde_json::json!({ "status": "closed" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "closed");

    // Reopen
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
        serde_json::json!({ "status": "open" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "open");
}

#[sqlx::test(migrations = "./migrations")]
async fn issue_auto_increment_numbers(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_a = helpers::create_project(&app, &admin_token, "auto-a", "public").await;
    let project_b = helpers::create_project(&app, &admin_token, "auto-b", "public").await;

    // Create 3 issues in project A
    for i in 1..=3 {
        let (_, body) = helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_a}/issues"),
            serde_json::json!({ "title": format!("A-{i}") }),
        )
        .await;
        assert_eq!(body["number"], i);
    }

    // Create 2 issues in project B — numbers should start at 1
    for i in 1..=2 {
        let (_, body) = helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_b}/issues"),
            serde_json::json!({ "title": format!("B-{i}") }),
        )
        .await;
        assert_eq!(body["number"], i);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn add_issue_comment(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "comment-proj", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "Commented Issue" }),
    )
    .await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        serde_json::json!({ "body": "A comment" }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["body"], "A comment");
}

#[sqlx::test(migrations = "./migrations")]
async fn create_merge_request(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-proj", "public").await;

    // Create a branch in the git repo for the MR source
    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();

    // Create a dummy branch in the bare repo
    tokio::process::Command::new("git")
        .args(["branch", "feature-branch", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests"),
        serde_json::json!({
            "source_branch": "feature-branch",
            "target_branch": "main",
            "title": "First MR",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["number"], 1);
    assert_eq!(body["title"], "First MR");
    assert_eq!(body["status"], "open");
}

#[sqlx::test(migrations = "./migrations")]
async fn list_merge_requests(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "list-mr", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();

    // Create two branches
    for branch in ["feat-1", "feat-2"] {
        tokio::process::Command::new("git")
            .args(["branch", branch, "main"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();
    }

    for (branch, title) in [("feat-1", "MR 1"), ("feat-2", "MR 2")] {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/merge-requests"),
            serde_json::json!({
                "source_branch": branch,
                "target_branch": "main",
                "title": title,
            }),
        )
        .await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().len() >= 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_merge_request(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "upd-mr", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();

    tokio::process::Command::new("git")
        .args(["branch", "upd-branch", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests"),
        serde_json::json!({
            "source_branch": "upd-branch",
            "target_branch": "main",
            "title": "Original MR Title",
        }),
    )
    .await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        serde_json::json!({ "title": "Updated MR Title" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Updated MR Title");
}

#[sqlx::test(migrations = "./migrations")]
async fn add_mr_comment(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-comment", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();

    tokio::process::Command::new("git")
        .args(["branch", "comment-branch", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests"),
        serde_json::json!({
            "source_branch": "comment-branch",
            "target_branch": "main",
            "title": "MR with comments",
        }),
    )
    .await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        serde_json::json!({ "body": "LGTM!" }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["body"], "LGTM!");
}

#[sqlx::test(migrations = "./migrations")]
async fn issue_requires_project_read(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "private-issues", "private").await;

    // Create an issue as admin
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "Secret Issue" }),
    )
    .await;

    // User without access
    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "noaccess", "noaccess@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues/1"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND); // 404 not 403
}

#[sqlx::test(migrations = "./migrations")]
async fn issue_write_requires_project_write(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "write-issues", "public").await;

    // Create a viewer user
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "viewissues", "viewissues@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    // Viewer can read issues on public project (via require_project_read)
    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Viewer cannot create issues (requires project:write)
    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "Should Fail" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
