mod helpers;

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Seed a bare repo with an initial commit so branches can be created.
async fn seed_bare_repo(repo_path: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::process::Command::new("git")
        .args(["clone", repo_path, "work"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.local"])
        .current_dir(&work)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&work)
        .output()
        .unwrap();
    std::fs::write(work.join("README.md"), "# test\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&work)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&work)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["push", "origin", "HEAD:refs/heads/main"])
        .current_dir(&work)
        .output()
        .unwrap();
}

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

    // Seed with an initial commit so branches can be created
    seed_bare_repo(&repo_path).await;

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

    seed_bare_repo(&repo_path).await;

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

    seed_bare_repo(&repo_path).await;

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

    seed_bare_repo(&repo_path).await;

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

// ---------------------------------------------------------------------------
// MR Review tests
// ---------------------------------------------------------------------------

async fn get_user_id(app: &axum::Router, token: &str) -> Uuid {
    let (_, body) = helpers::get_json(app, token, "/api/auth/me").await;
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Insert an MR directly (bypassing branch checks) for review/comment tests.
async fn insert_mr(pool: &PgPool, project_id: Uuid, author_id: Uuid, number: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status)
          VALUES ($1, $2, $3, $4, 'feat', 'main', 'Test MR', 'open')"
    )
    .bind(id)
    .bind(project_id)
    .bind(number)
    .bind(author_id)
    .execute(pool)
    .await
    .unwrap();

    // Bump the project's next_mr_number so it doesn't collide
    sqlx::query("UPDATE projects SET next_mr_number = $1 WHERE id = $2")
        .bind(number + 1)
        .bind(project_id)
        .execute(pool)
        .await
        .unwrap();

    id
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_list_reviews_empty(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "review-proj", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
    assert!(body["items"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_create_review_approve(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "approve-proj", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
        json!({ "verdict": "approve", "body": "LGTM" }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["verdict"], "approve");
    assert_eq!(body["body"], "LGTM");
    assert!(body["id"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_create_review_request_changes(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "changes-proj", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
        json!({ "verdict": "request_changes", "body": "Please fix" }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["verdict"], "request_changes");
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_create_review_invalid_verdict(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "bad-verdict", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
        json!({ "verdict": "reject" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// MR Comment tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn mr_list_comments_empty(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-list", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_create_and_list_comments(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-create", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": "First comment" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["body"], "First comment");

    // List
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_update_comment_by_author(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-update", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (_, comment) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": "Original" }),
    )
    .await;
    let comment_id = comment["id"].as_str().unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
        json!({ "body": "Updated" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["body"], "Updated");
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_update_comment_non_author_forbidden(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-forbid", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Admin creates a comment
    let (_, comment) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": "Admin's comment" }),
    )
    .await;
    let comment_id = comment["id"].as_str().unwrap();

    // Another user tries to update it
    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "other-user", "other@test.com").await;

    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
        json!({ "body": "Hijacked" }),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_update_comment_by_admin(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-admin", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Create a user and have them post a comment
    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "commenter", "commenter@test.com").await;

    let (_, comment) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": "User comment" }),
    )
    .await;
    let comment_id = comment["id"].as_str().unwrap();

    // Admin can update anyone's comment
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
        json!({ "body": "Edited by admin" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["body"], "Edited by admin");
}

// ---------------------------------------------------------------------------
// Additional MR coverage tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn mr_close_and_reopen(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-close-reopen", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Close the MR
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "status": "closed" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "closed");

    // Reopen the MR
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "status": "open" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "open");
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_update_description(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-desc", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Update both title and body
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({
            "title": "Updated Title",
            "body": "This is the new description with details."
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Updated Title");
    assert_eq!(body["body"], "This is the new description with details.");
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_update_invalid_status(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-bad-status", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Try to set status to "merged" via update (should require merge endpoint)
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "status": "merged" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_list_private_project_non_member(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id =
        helpers::create_project(&app, &admin_token, "mr-private-list", "private").await;

    // Insert an MR as admin
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Create a user with no access to this private project
    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "mr-noaccess", "mrnoaccess@test.com").await;

    // Non-member should get 404 (not 403) to avoid leaking existence
    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_get_private_project_non_member(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-priv-get", "private").await;

    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "mrnoget", "mrnoget@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_create_review_comment_verdict(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "review-comment", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
        json!({ "verdict": "comment", "body": "Just a thought" }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["verdict"], "comment");
    assert_eq!(body["body"], "Just a thought");
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_list_reviews_with_multiple(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "multi-reviews", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Create multiple reviews
    for (verdict, body_text) in [
        ("comment", "Looks interesting"),
        ("request_changes", "Please fix the typo"),
        ("approve", "Ship it!"),
    ] {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
            json!({ "verdict": verdict, "body": body_text }),
        )
        .await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 3);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_update_non_author_without_project_write(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-noauth", "public").await;

    // Admin creates the MR
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    // Create a viewer user (no project:write permission)
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "mrviewer", "mrviewer@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    // Viewer (non-author, no project:write) should be forbidden from updating
    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "title": "Hijacked Title" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_comment_empty_body_rejected(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-empty-cmt", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": "" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_update_comment_not_found(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmt-404", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let fake_comment_id = Uuid::new_v4();
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{fake_comment_id}"),
        json!({ "body": "updating nothing" }),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// MR list filter tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn mr_list_filter_by_status(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-filt-st", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;

    insert_mr(&pool, project_id, admin_id, 1).await;
    insert_mr(&pool, project_id, admin_id, 2).await;

    // Close MR #2
    helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/2"),
        json!({ "status": "closed" }),
    )
    .await;

    // Filter by status=open
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests?status=open"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["number"], 1);

    // Filter by status=closed
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests?status=closed"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["number"], 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_list_filter_by_author(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-filt-auth", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;

    let (other_id, _) =
        helpers::create_user(&app, &admin_token, "mr-other-auth", "mrother@test.com").await;

    insert_mr(&pool, project_id, admin_id, 1).await;

    let mr2_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_requests (id, project_id, number, author_id, source_branch, target_branch, title, status)
         VALUES ($1, $2, 2, $3, 'feat-2', 'main', 'Other MR', 'open')"
    )
    .bind(mr2_id).bind(project_id).bind(other_id)
    .execute(&pool).await.unwrap();
    sqlx::query("UPDATE projects SET next_mr_number = 3 WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests?author_id={admin_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["number"], 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_list_with_pagination(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-filt-page", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;

    for i in 1..=5 {
        insert_mr(&pool, project_id, admin_id, i).await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests?limit=2&offset=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 5);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests?limit=2&offset=4"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 5);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// MR create validation tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn mr_create_same_branch_rejected(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-same-br", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();
    seed_bare_repo(&repo_path).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests"),
        json!({
            "source_branch": "main",
            "target_branch": "main",
            "title": "Same branch MR",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_create_nonexistent_source_branch(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-no-src", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();
    seed_bare_repo(&repo_path).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests"),
        json!({
            "source_branch": "nonexistent-branch",
            "target_branch": "main",
            "title": "Ghost branch MR",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn mr_get_not_found(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-get-404", "public").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Merge endpoint tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn merge_already_closed_mr(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-merge-closed", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "status": "closed" }),
    )
    .await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn merge_nonexistent_mr(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-merge-404", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/merge"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn merge_forbidden_for_viewer(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "mr-merge-viewer", "public").await;
    let admin_id = get_user_id(&app, &admin_token).await;
    insert_mr(&pool, project_id, admin_id, 1).await;

    let (user_id, user_token) = helpers::create_user(
        &app,
        &admin_token,
        "mr-viewer-merge",
        "mrviewermerge@test.com",
    )
    .await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1/merge"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Review/comment edge cases
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn review_on_nonexistent_mr(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "review-no-mr", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/reviews"),
        json!({ "verdict": "approve" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn comment_on_nonexistent_mr(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-no-mr", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/comments"),
        json!({ "body": "Comment on nothing" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn review_list_on_nonexistent_mr(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "rev-list-404", "public").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/reviews"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn comment_list_on_nonexistent_mr(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let project_id = helpers::create_project(&app, &admin_token, "cmt-list-404", "public").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/comments"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
