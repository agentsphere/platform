mod helpers;

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Seed a bare repo with an initial commit so branches can be created.
fn seed_bare_repo(repo_path: &str) {
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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
    assert_eq!(body["total"].as_i64().unwrap(), 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_issue_by_number(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
async fn issue_comment_empty_body_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-empty-cmt", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "Issue for empty comment" }),
    )
    .await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        json!({ "body": "" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_merge_request(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-proj", "public").await;

    // Create a branch in the git repo for the MR source
    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();

    // Seed with an initial commit so branches can be created
    seed_bare_repo(&repo_path);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "list-mr", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();

    seed_bare_repo(&repo_path);

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
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_merge_request(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "upd-mr", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();

    seed_bare_repo(&repo_path);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-comment", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();

    seed_bare_repo(&repo_path);

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
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-same-br", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();
    seed_bare_repo(&repo_path);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-no-src", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();
    seed_bare_repo(&repo_path);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

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
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "cmt-list-404", "public").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/comments"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Issue edge cases
// ---------------------------------------------------------------------------

/// Update issue with maximum labels (50) succeeds.
#[sqlx::test(migrations = "./migrations")]
async fn update_issue_max_labels(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let project_id = helpers::create_project(&app, &admin_token, "maxlabels", "public").await;

    // Create issue
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "label test" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let number = body["number"].as_i64().unwrap();

    // Update with 50 labels (max allowed)
    let labels: Vec<String> = (0..50).map(|i| format!("label-{i}")).collect();
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/{number}"),
        serde_json::json!({ "labels": labels }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "50 labels should be allowed: {body}"
    );
}

/// Update issue with 51 labels is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn update_issue_too_many_labels_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let project_id = helpers::create_project(&app, &admin_token, "overlabels", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "label overflow" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let labels: Vec<String> = (0..51).map(|i| format!("label-{i}")).collect();
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
        serde_json::json!({ "labels": labels }),
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "51 labels should be rejected, got {status}"
    );
}

/// Delete an issue comment by non-author non-admin is forbidden.
#[sqlx::test(migrations = "./migrations")]
async fn issue_delete_comment_non_author_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let project_id = helpers::create_project(&app, &admin_token, "cmt-del-auth", "public").await;

    // Create issue and comment as admin
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        serde_json::json!({ "title": "comment auth test" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        serde_json::json!({ "body": "admin comment" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let comment_id = body["id"].as_str().unwrap();

    // Create a non-admin user with project write access
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmt-deleter", "cmt-deleter@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        user_id,
        "developer",
        Some(project_id),
        &pool,
    )
    .await;

    // Try to delete admin's comment — should be forbidden
    let (status, _) = helpers::delete_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues/1/comments/{comment_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// MR delete + additional coverage
// ---------------------------------------------------------------------------

/// Delete an MR.
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let project_id = helpers::create_project(&app, &admin_token, "mr-delete", "private").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it's closed (DELETE closes the MR, doesn't hard-delete it)
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "closed", "MR should be closed after delete");
}

// ---------------------------------------------------------------------------
// MR create with auto_merge
// ---------------------------------------------------------------------------

/// POST with `auto_merge:true` stores `auto_merge=true` in DB.
#[sqlx::test(migrations = "./migrations")]
async fn mr_create_with_auto_merge(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-auto-merge", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();

    seed_bare_repo(&repo_path);

    tokio::process::Command::new("git")
        .args(["branch", "auto-branch", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests"),
        serde_json::json!({
            "source_branch": "auto-branch",
            "target_branch": "main",
            "title": "Auto Merge MR",
            "auto_merge": true,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["number"], 1);

    // Verify auto_merge is set in DB
    let am: (bool,) = sqlx::query_as(
        "SELECT auto_merge FROM merge_requests WHERE project_id = $1 AND number = 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(am.0, "auto_merge should be true in DB");
}

// ---------------------------------------------------------------------------
// Delete MR edge cases
// ---------------------------------------------------------------------------

/// Delete a merged MR returns 409 Conflict.
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr_already_merged_conflict(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-del-merged", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Set status to merged
    sqlx::query("UPDATE merge_requests SET status = 'merged' WHERE project_id = $1 AND number = 1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// Delete a nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr_nonexistent(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-del-404", "public").await;

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Viewer cannot delete an MR (requires project:write).
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr_forbidden_viewer(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-del-viewer", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "del-viewer", "delviewer@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _) = helpers::delete_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Get MR comment by ID
// ---------------------------------------------------------------------------

/// GET individual comment returns 200 with correct fields.
#[sqlx::test(migrations = "./migrations")]
async fn get_mr_comment_by_id(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmt-get", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Insert comment directly
    let comment_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO comments (id, project_id, mr_id, author_id, body) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(comment_id)
    .bind(project_id)
    .bind(mr_id)
    .bind(admin_id)
    .bind("Test comment body")
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], comment_id.to_string());
    assert_eq!(body["body"], "Test comment body");
    assert_eq!(body["author_id"], admin_id.to_string());
    assert!(body["created_at"].is_string());
    assert!(body["updated_at"].is_string());
}

/// GET nonexistent comment returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_mr_comment_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmt-get404", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Delete MR comment edge cases
// ---------------------------------------------------------------------------

/// Non-author (with project:write but not admin) cannot delete another user's comment.
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr_comment_non_author_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmtdel-auth", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Admin creates a comment
    let comment_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO comments (id, project_id, mr_id, author_id, body) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(comment_id)
    .bind(project_id)
    .bind(mr_id)
    .bind(admin_id)
    .bind("admin's comment")
    .execute(&pool)
    .await
    .unwrap();

    // Create a non-admin user with project:write (developer)
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmtdel-dev", "cmtdeldev@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        user_id,
        "developer",
        Some(project_id),
        &pool,
    )
    .await;

    // Developer tries to delete admin's comment — should be forbidden
    let (status, _) = helpers::delete_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Delete a nonexistent MR comment returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr_comment_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmtdel-404", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// MR validation limits
// ---------------------------------------------------------------------------

/// MR title > 500 chars is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn mr_create_title_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-title-long", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();
    seed_bare_repo(&repo_path);

    tokio::process::Command::new("git")
        .args(["branch", "title-long", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();

    let long_title = "a".repeat(501);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests"),
        json!({
            "source_branch": "title-long",
            "target_branch": "main",
            "title": long_title,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// MR body > 100000 chars is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn mr_create_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-body-long", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();
    seed_bare_repo(&repo_path);

    tokio::process::Command::new("git")
        .args(["branch", "body-long", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();

    let long_body = "b".repeat(100_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests"),
        json!({
            "source_branch": "body-long",
            "target_branch": "main",
            "title": "Valid title",
            "body": long_body,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Update MR title > 500 chars is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn mr_update_title_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-tlong", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let long_title = "a".repeat(501);
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "title": long_title }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Update nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn mr_update_nonexistent_mr(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-upd-noexist", "public").await;

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999"),
        json!({ "title": "New title" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Review body > 100000 chars is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn mr_review_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-rev-blong", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let long_body = "c".repeat(100_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
        json!({ "verdict": "comment", "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Comment body > 100000 chars is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn mr_comment_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmt-blong", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let long_body = "d".repeat(100_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Issues: create with labels
// ===========================================================================

/// Create an issue with labels and verify they are returned.
#[sqlx::test(migrations = "./migrations")]
async fn create_issue_with_labels(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-labels", "public").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({
            "title": "Labeled issue",
            "body": "Has labels",
            "labels": ["bug", "urgent", "frontend"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let labels = body["labels"].as_array().unwrap();
    assert_eq!(labels.len(), 3);
    assert!(labels.iter().any(|l| l == "bug"));
    assert!(labels.iter().any(|l| l == "urgent"));
    assert!(labels.iter().any(|l| l == "frontend"));
}

// ===========================================================================
// Issues: update by non-author non-admin forbidden
// ===========================================================================

/// Non-author with project:write but without admin cannot update another user's issue.
#[sqlx::test(migrations = "./migrations")]
async fn update_issue_non_author_non_admin_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-upd-noauth", "public").await;

    // Admin creates an issue
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Admin's issue" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Create a non-admin developer user
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "issue-dev", "issuedev@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        user_id,
        "developer",
        Some(project_id),
        &pool,
    )
    .await;

    // Developer (non-author, not admin) tries to update admin's issue
    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues/1"),
        json!({ "title": "Hijacked title" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Admin can update another user's issue.
#[sqlx::test(migrations = "./migrations")]
async fn update_issue_by_admin_on_other_user_issue(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-admin-upd", "public").await;

    // Create a developer user who creates the issue
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "issue-author", "issueauthor@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        user_id,
        "developer",
        Some(project_id),
        &pool,
    )
    .await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "User's issue" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Admin updates user's issue
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
        json!({ "title": "Admin edited title" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Admin edited title");
}

// ===========================================================================
// Issues: update with invalid status
// ===========================================================================

/// Updating an issue with an invalid status returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn update_issue_invalid_status(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-bad-status", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Status test" }),
    )
    .await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
        json!({ "status": "invalid_status" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

/// Updating an issue status to "merged" (not valid for issues) returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn update_issue_status_merged_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-merged-st", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Status test merged" }),
    )
    .await;

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
        json!({ "status": "merged" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Issues: create comment with project:read only
// ===========================================================================

/// A user with project:read (not write) can create comments on issues.
#[sqlx::test(migrations = "./migrations")]
async fn issue_comment_with_read_only_permission(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Public project — anyone authenticated can read
    let project_id = helpers::create_project(&app, &admin_token, "issue-cmt-read", "public").await;

    // Admin creates an issue
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Commentable issue" }),
    )
    .await;

    // Create a viewer (read-only) user
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmt-reader", "cmtreader@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    // Viewer should be able to comment (create_comment requires project_read only)
    let (status, body) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        json!({ "body": "Viewer's comment" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["body"], "Viewer's comment");
}

// ===========================================================================
// Issues: update/delete comment by non-author (non-admin)
// ===========================================================================

/// Non-author with project:write cannot update another user's issue comment.
#[sqlx::test(migrations = "./migrations")]
async fn issue_update_comment_non_author_forbidden(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-cmt-upd-na", "public").await;

    // Admin creates issue and comment
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Comment auth test" }),
    )
    .await;

    let (_, comment_body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        json!({ "body": "Admin's comment" }),
    )
    .await;
    let comment_id = comment_body["id"].as_str().unwrap();

    // Create a non-admin developer
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmt-upd-dev", "cmtupddev@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        user_id,
        "developer",
        Some(project_id),
        &pool,
    )
    .await;

    // Developer tries to update admin's comment — forbidden
    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues/1/comments/{comment_id}"),
        json!({ "body": "Hijacked comment" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Admin can update another user's issue comment.
#[sqlx::test(migrations = "./migrations")]
async fn issue_update_comment_by_admin(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-cmt-admin-upd", "public").await;

    // Admin creates issue
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Admin comment edit" }),
    )
    .await;

    // Create developer user who posts a comment
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmt-dev-usr", "cmtdevusr@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        user_id,
        "developer",
        Some(project_id),
        &pool,
    )
    .await;

    let (_, comment_body) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        json!({ "body": "Developer's comment" }),
    )
    .await;
    let comment_id = comment_body["id"].as_str().unwrap();

    // Admin updates developer's comment — should succeed
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments/{comment_id}"),
        json!({ "body": "Edited by admin" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["body"], "Edited by admin");
}

// ===========================================================================
// Issues: list with status filter
// ===========================================================================

/// List issues filtered by status.
#[sqlx::test(migrations = "./migrations")]
async fn list_issues_filter_by_status(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-filt-status", "public").await;

    // Create 3 issues
    for i in 1..=3 {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/issues"),
            json!({ "title": format!("Issue {i}") }),
        )
        .await;
    }

    // Close issue #2
    helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/2"),
        json!({ "status": "closed" }),
    )
    .await;

    // Filter by status=open
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues?status=open"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2);
    let items = body["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i["status"] == "open"));

    // Filter by status=closed
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues?status=closed"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["number"], 2);
}

/// List issues filtered by `assignee_id`.
#[sqlx::test(migrations = "./migrations")]
async fn list_issues_filter_by_assignee(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-filt-assign", "public").await;

    let admin_id = helpers::admin_user_id(&pool).await;

    // Create a developer user
    let (dev_id, _dev_token) =
        helpers::create_user(&app, &admin_token, "issue-assignee", "assignee@test.com").await;

    // Create issue assigned to dev
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Assigned issue", "assignee_id": dev_id }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Create issue without assignee
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Unassigned issue" }),
    )
    .await;

    // Filter by assignee_id
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues?assignee_id={dev_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["title"], "Assigned issue");

    // Filter by admin_id (no issues assigned to admin)
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues?assignee_id={admin_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
}

// ===========================================================================
// Issues: list/get comments
// ===========================================================================

/// List comments on an issue.
#[sqlx::test(migrations = "./migrations")]
async fn issue_list_comments(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-list-cmt", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "List comments test" }),
    )
    .await;

    // Add 3 comments
    for i in 1..=3 {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/issues/1/comments"),
            json!({ "body": format!("Comment {i}") }),
        )
        .await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 3);
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
}

/// List comments on an issue with pagination.
#[sqlx::test(migrations = "./migrations")]
async fn issue_list_comments_pagination(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-cmt-page", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Paginated comments" }),
    )
    .await;

    for i in 1..=5 {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/issues/1/comments"),
            json!({ "body": format!("Comment {i}") }),
        )
        .await;
    }

    // Page 1: limit=2
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments?limit=2&offset=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 5);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    // Last page
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments?limit=2&offset=4"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 5);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

/// Get a single issue comment by ID.
#[sqlx::test(migrations = "./migrations")]
async fn issue_get_comment_by_id(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-cmt-getid", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Get comment test" }),
    )
    .await;

    let (_, comment_body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        json!({ "body": "Specific comment" }),
    )
    .await;
    let comment_id = comment_body["id"].as_str().unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments/{comment_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], comment_id);
    assert_eq!(body["body"], "Specific comment");
    assert!(body["created_at"].is_string());
    assert!(body["updated_at"].is_string());
}

/// Get nonexistent issue comment returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn issue_get_comment_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-cmt-404", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "No comment here" }),
    )
    .await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Comment on nonexistent issue returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn issue_comment_on_nonexistent_issue(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-cmt-noissue", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/999/comments"),
        json!({ "body": "Ghost comment" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// List comments on nonexistent issue returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn issue_list_comments_nonexistent_issue(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-cmt-list404", "public").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/999/comments"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Issues: delete (close) edge cases
// ===========================================================================

/// Delete (close) an issue via DELETE endpoint.
#[sqlx::test(migrations = "./migrations")]
async fn delete_issue_closes_it(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-del-close", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Delete me" }),
    )
    .await;

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it's closed
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "closed");
}

/// Delete already-closed issue is idempotent (returns 204).
#[sqlx::test(migrations = "./migrations")]
async fn delete_issue_already_closed_idempotent(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-del-idem", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Close me twice" }),
    )
    .await;

    // Close via PATCH first
    helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
        json!({ "status": "closed" }),
    )
    .await;

    // Delete already-closed issue — should still return 204
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// Get nonexistent issue returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_issue_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-get-404", "public").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/999"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Update nonexistent issue returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn update_issue_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-upd-404", "public").await;

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/999"),
        json!({ "title": "Ghost update" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Issue comment body > 100000 chars is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn issue_comment_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-cmt-toolong", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Long comment test" }),
    )
    .await;

    let long_body = "x".repeat(100_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        json!({ "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Update issue comment body > 100000 chars is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn issue_update_comment_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-cmt-upd-long", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Update comment long" }),
    )
    .await;

    let (_, comment_body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        json!({ "body": "Short" }),
    )
    .await;
    let comment_id = comment_body["id"].as_str().unwrap();

    let long_body = "y".repeat(100_001);
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments/{comment_id}"),
        json!({ "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Update issue comment on nonexistent issue returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn issue_update_comment_nonexistent_issue(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-cmt-upd-ni", "public").await;

    let fake_comment_id = Uuid::new_v4();
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/999/comments/{fake_comment_id}"),
        json!({ "body": "update nothing" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Delete issue comment on nonexistent issue returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn issue_delete_comment_nonexistent_issue(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-cmt-del-ni", "public").await;

    let fake_comment_id = Uuid::new_v4();
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/999/comments/{fake_comment_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Delete issue comment by non-author admin succeeds.
#[sqlx::test(migrations = "./migrations")]
async fn issue_delete_comment_by_admin(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-cmt-del-adm", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Admin delete cmt" }),
    )
    .await;

    // Create developer who makes a comment
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "cmt-del-dev", "cmtdeldev@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        user_id,
        "developer",
        Some(project_id),
        &pool,
    )
    .await;

    let (_, comment_body) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        json!({ "body": "Dev's comment" }),
    )
    .await;
    let comment_id = comment_body["id"].as_str().unwrap();

    // Admin deletes developer's comment
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments/{comment_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// Delete nonexistent issue comment returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn issue_delete_comment_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-cmt-del-404", "public").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Delete missing comment" }),
    )
    .await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1/comments/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Issues list with pagination.
#[sqlx::test(migrations = "./migrations")]
async fn list_issues_pagination(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-pagination", "public").await;

    for i in 1..=5 {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/issues"),
            json!({ "title": format!("Issue {i}") }),
        )
        .await;
    }

    // Page 1
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues?limit=2&offset=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 5);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    // Last page
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues?limit=2&offset=4"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 5);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

/// Issue body > 100000 chars on create is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_issue_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-body-long", "public").await;

    let long_body = "z".repeat(100_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Long body issue", "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// MR: Delete already-closed MR is idempotent
// ===========================================================================

/// Delete an already-closed MR returns 204 (idempotent).
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr_already_closed_idempotent(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-del-closed", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Close via PATCH
    helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "status": "closed" }),
    )
    .await;

    // Delete already-closed MR — should still return 204
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ===========================================================================
// MR: Get review not found
// ===========================================================================

/// GET nonexistent review returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_mr_review_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-rev-get404", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let fake_review_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews/{fake_review_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// MR: Admin can delete another user's comment
// ===========================================================================

/// Admin can delete another user's MR comment.
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr_comment_by_admin(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmt-del-adm", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Create developer user who posts a comment
    let (user_id, _user_token) =
        helpers::create_user(&app, &admin_token, "mr-cmt-devdel", "mrcmtdevdel@test.com").await;

    // Insert comment from developer directly
    let comment_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO comments (id, project_id, mr_id, author_id, body) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(comment_id)
    .bind(project_id)
    .bind(mr_id)
    .bind(user_id)
    .bind("developer comment")
    .execute(&pool)
    .await
    .unwrap();

    // Admin deletes developer's comment
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify deletion
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM comments WHERE id = $1")
        .bind(comment_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 0);
}

// ===========================================================================
// MR: Review without body (optional field)
// ===========================================================================

/// Create a review without body field.
#[sqlx::test(migrations = "./migrations")]
async fn mr_create_review_without_body(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-rev-nobody", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
        json!({ "verdict": "approve" }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["verdict"], "approve");
    assert!(body["body"].is_null());
}

// ===========================================================================
// MR: Update comment body too long
// ===========================================================================

/// Update MR comment body > 100000 chars is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn mr_update_comment_body_too_long(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmt-upd-long", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Insert comment
    let comment_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO comments (id, project_id, mr_id, author_id, body) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(comment_id)
    .bind(project_id)
    .bind(mr_id)
    .bind(admin_id)
    .bind("short")
    .execute(&pool)
    .await
    .unwrap();

    let long_body = "w".repeat(100_001);
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
        json!({ "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// MR: Update MR body too long
// ===========================================================================

/// Update MR body > 100000 chars is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn mr_update_body_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "mr-upd-body-long", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let long_body = "e".repeat(100_001);
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "body": long_body }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Issue: create with assignee
// ===========================================================================

/// Create an issue with an `assignee_id`.
#[sqlx::test(migrations = "./migrations")]
async fn create_issue_with_assignee(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-assignee", "public").await;

    let (dev_id, _) =
        helpers::create_user(&app, &admin_token, "assign-user", "assignuser@test.com").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({
            "title": "Assigned issue",
            "assignee_id": dev_id,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["assignee_id"], dev_id.to_string());
}

// ===========================================================================
// Issue: update with assignee
// ===========================================================================

/// Update issue `assignee_id`.
#[sqlx::test(migrations = "./migrations")]
async fn update_issue_assignee(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-upd-assign", "public").await;

    let (dev_id, _) =
        helpers::create_user(&app, &admin_token, "assign-upd-u", "assignupdu@test.com").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "No assignee" }),
    )
    .await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues/1"),
        json!({ "assignee_id": dev_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["assignee_id"], dev_id.to_string());
}

// ===========================================================================
// MR: Comment/review on private project by non-member
// ===========================================================================

/// Comment on MR in private project by non-member returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn mr_comment_private_project_non_member(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmt-priv", "private").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "mr-cmt-nopriv", "mrcmtnopriv@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments"),
        json!({ "body": "Unauthorized comment" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Review on MR in private project by non-member returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn mr_review_private_project_non_member(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-rev-priv", "private").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "mr-rev-nopriv", "mrrevnopriv@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
        json!({ "verdict": "approve" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Issue comment on private project by non-member returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn issue_comment_private_project_non_member(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "issue-cmt-priv", "private").await;

    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Private issue" }),
    )
    .await;

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "cmt-nopriv", "cmtnopriv@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues/1/comments"),
        json!({ "body": "Unauthorized comment" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Create issue on private project by non-member returns 403.
#[sqlx::test(migrations = "./migrations")]
async fn create_issue_private_project_non_member(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-create-priv", "private").await;

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "issue-nopriv", "issuenopriv@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Unauthorized issue" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Create issue with too many labels (51) is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_issue_too_many_labels_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id =
        helpers::create_project(&app, &admin_token, "issue-label-over", "public").await;

    let labels: Vec<String> = (0..51).map(|i| format!("label-{i}")).collect();
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/issues"),
        json!({ "title": "Too many labels", "labels": labels }),
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "51 labels on create should be rejected, got {status}"
    );
}

// ===========================================================================
// Auto-merge enable/disable endpoints
// ===========================================================================

/// PUT auto-merge on an open MR returns 200.
#[sqlx::test(migrations = "./migrations")]
async fn mr_enable_auto_merge(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-am-enable", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify auto_merge is true in DB
    let am: (bool,) = sqlx::query_as(
        "SELECT auto_merge FROM merge_requests WHERE project_id = $1 AND number = 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(am.0, "auto_merge should be true");
}

/// PUT auto-merge with explicit merge_method stores it.
#[sqlx::test(migrations = "./migrations")]
async fn mr_enable_auto_merge_with_method(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-am-method", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
        json!({ "merge_method": "squash" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let method: (Option<String>,) = sqlx::query_as(
        "SELECT auto_merge_method FROM merge_requests WHERE project_id = $1 AND number = 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(method.0.as_deref(), Some("squash"));
}

/// DELETE auto-merge disables it.
#[sqlx::test(migrations = "./migrations")]
async fn mr_disable_auto_merge(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-am-disable", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Enable first
    helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
        json!({}),
    )
    .await;

    // Disable
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify auto_merge is false
    let am: (bool,) = sqlx::query_as(
        "SELECT auto_merge FROM merge_requests WHERE project_id = $1 AND number = 1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!am.0, "auto_merge should be false");
}

/// Enable auto-merge on nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn mr_enable_auto_merge_nonexistent(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-am-404", "public").await;

    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/auto-merge"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Disable auto-merge on nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn mr_disable_auto_merge_nonexistent(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-am-dis404", "public").await;

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/auto-merge"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Enable auto-merge on closed MR returns 404 (WHERE status='open' doesn't match).
#[sqlx::test(migrations = "./migrations")]
async fn mr_enable_auto_merge_closed_mr(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-am-closed", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Close the MR
    helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "status": "closed" }),
    )
    .await;

    // Try to enable auto-merge on closed MR
    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Viewer cannot enable auto-merge (requires project:write).
#[sqlx::test(migrations = "./migrations")]
async fn mr_enable_auto_merge_forbidden_viewer(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-am-viewer", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "am-viewer", "amviewer@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _) = helpers::put_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/merge-requests/1/auto-merge"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ===========================================================================
// Get review by ID
// ===========================================================================

/// GET individual review returns correct fields.
#[sqlx::test(migrations = "./migrations")]
async fn get_review_by_id(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "rev-get-id", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Create a review
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews"),
        json!({ "verdict": "approve", "body": "Ship it" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let review_id = body["id"].as_str().unwrap();

    // GET it
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews/{review_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], review_id);
    assert_eq!(body["verdict"], "approve");
    assert_eq!(body["body"], "Ship it");
    assert_eq!(body["mr_id"], mr_id.to_string());
}

/// GET nonexistent review returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_review_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "rev-get-404", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/reviews/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// GET review on nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_review_nonexistent_mr(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "rev-get-nomr", "public").await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/reviews/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// MR delete comment by author succeeds
// ===========================================================================

/// Author can delete their own MR comment.
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr_comment_by_author(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmtdel-ok", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Admin creates a comment
    let comment_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO comments (id, project_id, mr_id, author_id, body) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(comment_id)
    .bind(project_id)
    .bind(mr_id)
    .bind(admin_id)
    .bind("to be deleted")
    .execute(&pool)
    .await
    .unwrap();

    // Author (admin) deletes their own comment
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it's gone
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1/comments/{comment_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Delete MR comment on nonexistent MR returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn delete_mr_comment_nonexistent_mr(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-cmtdel-nomr", "public").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/999/comments/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// MR update body > 100000 chars via PATCH
// ===========================================================================
// MR create with empty title
// ===========================================================================

/// MR create with empty title is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn mr_create_empty_title_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-empty-title", "public").await;

    let row: (Option<String>,) = sqlx::query_as("SELECT repo_path FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_path = row.0.unwrap();
    seed_bare_repo(&repo_path);

    tokio::process::Command::new("git")
        .args(["branch", "empty-title", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests"),
        json!({
            "source_branch": "empty-title",
            "target_branch": "main",
            "title": "",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Delete an already-closed MR is idempotent
// ===========================================================================

/// Delete an already-closed MR succeeds (idempotent close).
#[sqlx::test(migrations = "./migrations")]
async fn delete_already_closed_mr(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "mr-del-closed", "public").await;
    let admin_id = helpers::admin_user_id(&pool).await;
    let _mr_id = helpers::insert_mr(&pool, project_id, admin_id, "feat", "main", 1).await;

    // Close first
    helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
        json!({ "status": "closed" }),
    )
    .await;

    // Delete already-closed MR should still succeed
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/1"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
